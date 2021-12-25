varnish::boilerplate!();

use std::borrow::Cow;
use std::cmp::max;
use std::os::raw::c_void;
use std::ptr;
use std::slice;
use std::str::{from_utf8, from_utf8_unchecked};
use std::sync::Mutex;
use std::io::Write;

use lru::LruCache;
use regex::bytes::Regex;

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
    mutexed_cache: Mutex<LruCache<String, Result<Regex, String>>>,
}

const PRIV_ANCHOR: [u8; 1] = [0];

pub struct Captures<'a> {
    caps: regex::bytes::Captures<'a>,
    #[allow(dead_code)]
    text: Option<Box<Vec<u8>>>,
    #[allow(dead_code)]
    slice: Option<&'a [u8]>,
}

impl init {
    pub fn new(_ctx: &Ctx, _vcl_name: &str, opt_sz: Option<i64>) -> Self {
        let sz = max(0, opt_sz.unwrap_or(1000));
        init {
            mutexed_cache: Mutex::new(LruCache::new(sz as usize)),
        }
    }

    fn get_regex(&self, res: &str) -> Result<Regex, String> {
        let mut lru = self.mutexed_cache.lock().unwrap();
        if lru.get(res).is_none() {
            let comp = Regex::new(res).map_err(|e| e.to_string());
            lru.put(res.to_string(), comp);
        }
        lru.get(res).unwrap().clone()
    }

    pub fn is_match(&self, _: &mut Ctx, s: &str, res: &str) -> bool {
        self.get_regex(res)
            .map(|re| re.is_match(s.as_bytes()))
            .unwrap_or(false)
    }

    pub fn replace(
        &self,
        ctx: &mut Ctx,
        s: &str,
        res: &str,
        sub: &str,
        opt_lim: Option<i64>,
    ) -> Result<VCL_STRING, String> {
        let lim = max(0, opt_lim.unwrap_or(0));
        match self.get_regex(res) {
            Err(_) => s.into_vcl(&mut ctx.ws),
            Ok(re) => {
                let replaced = re.replacen(s.as_bytes(), lim as usize, sub.as_bytes());
                let buf = ctx.ws.copy_bytes_with_null(&replaced)?;
                Ok(buf.as_ptr() as VCL_STRING)
            },
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
            Ok(re) => re,
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
        // from_raw_parts is; we need rust to trust us on the lifetime of slice (which caps will
        // points to), so we go to raw parts and back again to trick it. It's not awesome, but it
        // works
        let ptr = text.as_ptr();
        let len = text.len();
        let slice = unsafe { slice::from_raw_parts(ptr, len) };
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
            Ok(re) => re,
        };

        let caps = match re.captures(s.as_bytes()) {
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

    pub fn group<'a>(&self, ctx: &mut Ctx, vp: &mut VPriv<Captures<'a>>, n: i64) -> Result<VCL_STRING, String> {
        let n = if n >= 0 { n } else { 0 } as usize;
        let cap_opt = vp.as_ref().and_then(|c| c.caps.get(n));
        if cap_opt.is_none() {
            return Ok(ptr::null());
        }
        Ok(ctx.ws.copy_bytes_with_null(&cap_opt.unwrap().as_bytes())?.as_ptr() as VCL_STRING)
    }

    pub fn named_group<'a>(
        &self,
        ctx: &mut Ctx,
        vp: &mut VPriv<Captures<'a>>,
        name: &str,
    ) -> Result<VCL_STRING, String> {
        let cap_opt = vp.as_ref().and_then(|c| c.caps.name(name));
        if cap_opt.is_none() {
            return Ok(ptr::null());
        }
        Ok(ctx.ws.copy_bytes_with_null(&cap_opt.unwrap().as_bytes())?.as_ptr() as VCL_STRING)
    }

    pub fn replace_resp_body(&self, ctx: &mut Ctx, res: &str, sub: &str) {
        let re = match self.get_regex(res) {
            Err(s) => {
                ctx.log(LogTag::VclError, &s);
                return ();
            }
            Ok(re) => re,
        };
        let priv_opt = unsafe {
            varnish_sys::VRT_priv_task(ctx.raw, PRIV_ANCHOR.as_ptr() as *const c_void).as_mut()
        };
        if priv_opt.is_none() {
            ctx.fail("rers: couldn't retrieve priv_task (workspace too small?)");
            return ();
        }
        let mut vp: VPriv<DeliveryReplacer> = VPriv::new(priv_opt.unwrap());
        if let Some(ri) = vp.as_mut() {
            ri.steps.push((re, sub.to_owned()));
        } else {
            let ri = DeliveryReplacer {
                body: Vec::new(),
                steps: vec![(re, sub.to_owned())],
            };
            vp.store(ri);
        }
    }
}

#[derive(Default)]
struct DeliveryReplacer {
    steps: Vec<(Regex, String)>,
    body: Vec<u8>,
}

impl OutProc for DeliveryReplacer {
    fn new(ctx: &mut OutCtx, _oc: *mut varnish_sys::objcore) -> Option<Self> {
        let privp;
        // we don't know how/if the body will be modified, so we nuke the content-length
        // it's also no worth fleshing out a rust object just to remove a header, we just use the C functions
        unsafe {
            let req = ctx.raw.req.as_ref().unwrap();
            assert_eq!(req.magic, varnish_sys::REQ_MAGIC);
            varnish_sys::http_Unset((*ctx.raw.req).resp, varnish_sys::H_Content_Length.as_ptr());

            // the lying! the cheating!
            let mut fake_ctx = TestCtx::new(0);
            fake_ctx.ctx().raw.req = ctx.raw.req;
            privp = varnish_sys::VRT_priv_task_get(
                fake_ctx.ctx().raw,
                PRIV_ANCHOR.as_ptr() as *const c_void,
            )
            .as_mut()?;
        }

        VPriv::new(privp).take()
    }

    fn bytes(&mut self, ctx: &mut OutCtx, act: OutAction, buf: &[u8]) -> OutResult {
        self.body.extend_from_slice(buf);

        if let OutAction::End = act {
            // if it's not a proper string, bailout
            let mut replaced_body = Cow::from(&self.body);
            for (re, sub) in &self.steps {
                // ignore the `Cow::Borrowed` case, it means nothing changed
                if let Cow::Owned(s) = re.replace(&replaced_body, sub.as_bytes()) {
                    replaced_body = Cow::from(s);
                }
            }
            ctx.push_bytes(act, &replaced_body)
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
