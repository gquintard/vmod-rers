varnish::boilerplate!();

use std::borrow::Cow;
use std::os::raw::c_void;
use std::slice;
use std::str::{from_utf8, from_utf8_unchecked};
use std::sync::Mutex;

use varnish::vcl::convert::IntoVCL;
use varnish::vcl::ctx::{Ctx, Event, LogTag, TestCtx};
use varnish::vcl::processor::{new_vdp, OutAction, OutCtx, OutProc, OutResult};
use varnish::vcl::vpriv::VPriv;
use varnish_sys::VCL_STRING;

varnish::vtc!(test01);
varnish::vtc!(test02);
varnish::vtc!(test03);
varnish::vtc!(test04);
varnish::vtc!(test05);
varnish::vtc!(test06);

#[allow(non_camel_case_types)]
pub struct init {
    mutexed_cache: Mutex<regex_cache::RegexCache>,
}

const PRIV_ANCHOR: [u8; 1] = [0];

struct ReplaceSteps {
    v: Vec<(regex_cache::Regex, String)>,
}

pub struct Captures<'a> {
    caps: regex::Captures<'a>,
    #[allow(dead_code)]
    text: Option<Box<Vec<u8>>>,
    #[allow(dead_code)]
    slice: Option<&'a str>,
}

impl init {
    pub fn new(_ctx: &Ctx, _vcl_name: &str, opt_sz: Option<i64>) -> Self {
        let sz = match opt_sz {
            Some(n) if n > 0 => n,
            _ => 1000,
        };
        init {
            mutexed_cache: Mutex::new(regex_cache::RegexCache::new(sz as usize)),
        }
    }

    fn get_regex(&self, res: &str) -> Result<regex_cache::Regex, String> {
        self.mutexed_cache
            .lock()
            .unwrap()
            .compile(res)
            .map(|re| re.clone())
            .map_err(|e| e.to_string())
    }

    pub fn is_match(&self, _: &mut Ctx, s: &str, res: &str) -> bool {
        match self.get_regex(res) {
            Err(_) => false,
            Ok(re) => re.is_match(s),
        }
    }

    pub fn replace(
        &self,
        ctx: &mut Ctx,
        s: &str,
        res: &str,
        sub: &str,
        opt_lim: Option<i64>,
    ) -> Result<VCL_STRING, String> {
        let lim = match opt_lim {
            Some(n) if n >= 0 => n,
            _ => 0,
        };
        match self.get_regex(res) {
            Err(_) => s.into_vcl(&mut ctx.ws),
            Ok(re) => Ok(re.replacen(s, lim as usize, sub).into_vcl(&mut ctx.ws)?),
        }
    }

    pub fn capture_req_body<'a>(
        &self,
        ctx: &mut Ctx,
        vp: &mut VPriv<Captures<'a>>,
        res: &str,
    ) -> Result<bool, String> {
        vp.clear();

        let re = match self.get_regex(res) {
            Err(_) => return Ok(false),
            Ok(re) => re.clone(),
        };

        // we need a contiguous buffer to present to the regex, so we coalesce the cached body
        let body = ctx
            .cached_req_body()?
            .into_iter()
            .fold(Vec::new(), |mut v, b| {
                v.extend_from_slice(b);
                v
            });

        // make sure it's valid UTF8
        if from_utf8(body.as_slice()).is_err() {
            ctx.log(LogTag::VclError, "regex: request body isn't proper utf8");
            return Ok(false);
        }

        // put the body on the heap so we can trust pointers to it
        let text = Box::new(body);

        // from_utf8_unchecked isn't unsafe, as we already checked with from_utf8(), but
        // from_raw_parts is we need rust to trust us on the lifetime of slice (which caps will
        // points to), so we go to raw parts and back again to trick it. It's not awesome, but it
        // works
        let ptr = text.as_ptr();
        let len = text.len();
        let slice = unsafe { from_utf8_unchecked(slice::from_raw_parts(ptr, len)) };
        match re.captures(slice) {
            None => Ok(false),
            Some(caps) => {
                vp.store(Captures {
                    caps,
                    text: Some(text),
                    slice: Some(slice),
                });
                Ok(true)
            }
        }
    }

    pub fn capture<'a>(
        &self,
        _: &mut Ctx,
        vp: &mut VPriv<Captures<'a>>,
        s: &'a str,
        res: &str,
    ) -> bool {
        vp.clear();

        let re = match self.get_regex(res) {
            Err(_) => return false,
            Ok(re) => re.clone(),
        };

        let caps = match re.captures(s) {
            None => return false,
            Some(caps) => caps,
        };
        vp.store(Captures {
            caps,
            text: None,
            slice: None,
        });
        true
    }

    pub fn group<'a>(&self, _: &mut Ctx, vp: &mut VPriv<Captures<'a>>, n: i64) -> &'a str {
        let n = if n >= 0 { n } else { 0 } as usize;
        vp.as_ref()
            .and_then(|c| c.caps.get(n))
            .map(|m| m.as_str())
            .unwrap_or("")
    }

    pub fn named_group<'a>(
        &self,
        _: &mut Ctx,
        vp: &mut VPriv<Captures<'a>>,
        name: &str,
    ) -> &'a str {
        vp.as_ref()
            .and_then(|c| c.caps.name(name))
            .map(|m| m.as_str())
            .unwrap_or("")
    }

    pub fn replace_resp_body(&self, ctx: &mut Ctx, res: &str, sub: &str) {
        let re = match self.get_regex(res) {
            Err(s) => {
                ctx.log(LogTag::VclError, &s);
                return ();
            }
            Ok(re) => re,
        };
        //TODO: check for VRT_priv_task failure
        let mut vp: VPriv<ReplaceSteps> = unsafe {
            VPriv::new(varnish_sys::VRT_priv_task(
                ctx.raw,
                PRIV_ANCHOR.as_ptr() as *const c_void,
            ))
        };
        if let Some(ri) = vp.as_mut() {
            ri.v.push((re, sub.to_owned()));
        } else {
            let ri = ReplaceSteps {
                v: vec![(re, sub.to_owned())],
            };
            vp.store(ri);
        }
    }
}

#[derive(Default)]
struct DeliveryReplacer {
    steps: Vec<(regex_cache::Regex, String)>,
    body: Vec<u8>,
}

impl OutProc for DeliveryReplacer {
    fn new(ctx: &mut OutCtx, _oc: *mut varnish_sys::objcore) -> Result<Self, String> {
        let priv_opt;
        // we don't know how/if the body will be modified, so we nuke the content-length
        // it's also no worth fleshing out a rust object just to remove a header, use the C function
        unsafe {
            let req = ctx.raw.req.as_ref().unwrap();
            assert_eq!(req.magic, varnish_sys::REQ_MAGIC);
            varnish_sys::http_Unset((*ctx.raw.req).resp, varnish_sys::H_Content_Length.as_ptr());

            // the lying! the cheating!
            let mut fake_ctx = TestCtx::new(0);
            fake_ctx.ctx().raw.req = ctx.raw.req;
            priv_opt = varnish_sys::VRT_priv_task_get(
                fake_ctx.ctx().raw,
                PRIV_ANCHOR.as_ptr() as *const c_void,
            )
            .as_mut()
        }

        if let Some(privp) = priv_opt {
            let mut vp: VPriv<ReplaceSteps> = VPriv::new(privp);
            Ok(DeliveryReplacer {
                steps: match vp.take() {
                    Some(mut rs) => std::mem::take(&mut rs.v),
                    None => Vec::new(),
                },
                body: Vec::new(),
            })
        } else {
            return Ok(Default::default());
        }
    }

    fn bytes(&mut self, ctx: &mut OutCtx, act: OutAction, buf: &[u8]) -> OutResult {
        self.body.extend_from_slice(buf);

        if let OutAction::End = act {
            // if it's not a proper string, bailout
            let str_body = match from_utf8(self.body.as_slice()) {
                Err(_) => {
                    // TODO: log error
                    return ctx.push_bytes(act, self.body.as_slice());
                }
                Ok(s) => s,
            };
            let mut replaced_body = Cow::from(str_body);
            for (re, sub) in &self.steps {
                // ignore the case where the resulting `String` is `Cow::Borrowed`, it means
                // nothing changed
                if let Cow::Owned(s) = re.replace(&replaced_body, sub) {
                    replaced_body = Cow::from(s);
                }
            }
            ctx.push_bytes(act, replaced_body.as_bytes())
        } else {
            OutResult::Continue
        }
    }

    fn name() -> &'static str {
        "rers\0"
    }
}

pub unsafe fn event(
    ctx: &mut Ctx,
    vp: &mut VPriv<varnish_sys::vdp>,
    event: Event,
) -> Result<(), &'static str> {
    match event {
        Event::Load => {
            vp.store(new_vdp::<DeliveryReplacer>());
            varnish_sys::VRT_AddVDP(ctx.raw, vp.as_ref().unwrap())
        }
        Event::Discard => varnish_sys::VRT_RemoveVDP(ctx.raw, vp.as_ref().unwrap()),
        _ => (),
    }
    Ok(())
}
