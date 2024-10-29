use std::borrow::Cow;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::sync::Mutex;

use lru::LruCache;
use regex::bytes::Regex;
use varnish::vcl::{Ctx, InitResult, PullResult, PushResult, VDPCtx, VFPCtx, VDP, VFP};
use varnish::{ffi, run_vtc_tests};
use varnish_sys::ffi::{vmod_priv, vmod_priv_methods, VdpAction, VMOD_PRIV_METHODS_MAGIC};

run_vtc_tests!("tests/*.vtc");

#[varnish::vmod]
mod rers {
    use std::cmp::max;
    use std::error::Error;
    use std::slice;
    use std::str::from_utf8;
    use std::sync::Mutex;

    use lru::LruCache;
    use varnish::ffi::{self, vdp, vfp};
    use varnish::vcl::{new_vdp, new_vfp, Ctx, Event, LogTag};

    use super::{init, Captures, Vxp, PRIV_ANCHOR, PRIV_VXP_METHODS};

    impl init {
        pub fn new(#[default(1000)] cache_size: i64) -> Self {
            let cache_size = max(0, cache_size) as usize;
            init {
                mutexed_cache: Mutex::new(LruCache::new(cache_size)),
            }
        }

        pub fn is_match(&self, s: &str, res: &str) -> bool {
            self.get_regex(res)
                .map(|re| re.is_match(s.as_bytes()))
                .unwrap_or(false)
        }

        pub fn replace(
            &self,
            haystack: &str,
            res: &str,
            sub: &str,
            limit: Option<i64>,
        ) -> Result<String, String> {
            let limit = max(0, limit.unwrap_or(0)) as usize;
            let re = self.get_regex(res)?;
            let repl = re.replacen(haystack.as_bytes(), limit, sub.as_bytes());
            from_utf8(repl.as_ref())
                .map_err(|e| e.to_string())
                .map(|s| s.to_owned())
        }

        pub fn capture_req_body(
            &self,
            ctx: &mut Ctx,
            #[shared_per_task] vp: &mut Option<Box<Captures<'_>>>,
            res: &str,
        ) -> Result<bool, Box<dyn Error>> {
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

            // we need rust to trust us on the lifetime of slice (which caps will
            // point to), so we go to raw parts and back again to trick it. It's not awesome, but it
            // works
            let ptr = body.as_ptr();
            let len = body.len();
            let slice = unsafe { slice::from_raw_parts(ptr, len) };
            match re.captures(slice) {
                None => Ok(false),
                Some(caps) => {
                    *vp = Some(Box::new(Captures {
                        caps,
                        text: Some(body),
                        slice: Some(slice),
                    }));
                    Ok(true)
                }
            }
        }

        pub fn capture<'a>(
            &self,
            #[shared_per_task] vp: &mut Option<Box<Captures<'a>>>,
            s: &'a str,
            res: &str,
        ) -> bool {
            let re = match self.get_regex(res) {
                Err(_) => return false,
                Ok(re) => re,
            };

            let caps = match re.captures(s.as_bytes()) {
                None => return false,
                Some(caps) => caps,
            };
            *vp = Some(Box::new(Captures {
                caps,
                text: None,
                slice: None,
            }));
            true
        }

        pub fn group<'a>(
            &self,
            #[shared_per_task] vp: &mut Option<Box<Captures<'a>>>,
            n: i64,
        ) -> Option<&'a [u8]> {
            let n = if n >= 0 { n } else { 0 } as usize;
            vp.as_ref()
                .and_then(|c| c.caps.get(n))
                .map(|m| m.as_bytes())
        }

        pub fn named_group<'a>(
            &self,
            #[shared_per_task] vp: &mut Option<Box<Captures<'a>>>,
            name: &str,
        ) -> Option<&'a [u8]> {
            vp.as_ref()
                .and_then(|c| c.caps.name(name))
                .map(|m| m.as_bytes())
        }

        pub fn replace_resp_body(&self, ctx: &mut Ctx, res: &str, sub: &str) {
            let Ok(re) = self
                .get_regex(res)
                .map_err(|e| ctx.log(LogTag::VclError, &e))
            else {
                return; // FIXME: should this return an error to call VRT_fail()?
            };

            let priv_opt = unsafe { ffi::VRT_priv_task(ctx.raw, PRIV_ANCHOR).as_mut() };
            let Some(priv_opt) = priv_opt else {
                ctx.fail("rers: couldn't retrieve priv_task (workspace too small?)");
                return;
            };

            // Low level access: convert pointer into a Box, manipulate it, and store it back
            let vp = unsafe { (*priv_opt).take::<Vxp>() };
            let value = (re, sub.to_owned());
            let ri = if let Some(mut ri) = vp {
                ri.steps.push(value);
                ri
            } else {
                Box::new(Vxp {
                    body: Vec::new(),
                    steps: vec![value],
                    sent: None,
                })
            };
            unsafe {
                (*priv_opt).put(ri, &PRIV_VXP_METHODS);
            }
        }
    }

    #[event]
    pub fn event(
        ctx: &mut Ctx,
        #[shared_per_vcl] vp: &mut Option<Box<(vfp, vdp)>>,
        event: Event,
    ) -> Result<(), String> {
        match event {
            Event::Load => {
                *vp = Some(Box::new((new_vfp::<Vxp>(), new_vdp::<Vxp>())));
                unsafe {
                    ffi::VRT_AddFilter(ctx.raw, &vp.as_ref().unwrap().0, &vp.as_ref().unwrap().1);
                }
            }
            Event::Discard => unsafe {
                ffi::VRT_RemoveFilter(ctx.raw, &vp.as_ref().unwrap().0, &vp.as_ref().unwrap().1);
            },
            _ => {}
        }
        Ok(())
    }
}

impl init {
    fn get_regex(&self, res: &str) -> Result<Regex, String> {
        let mut lru = self.mutexed_cache.lock().unwrap();
        if lru.get(res).is_none() {
            let comp = Regex::new(res).map_err(|e| e.to_string());
            lru.put(res.into(), comp);
        }
        lru.get(res).unwrap().clone()
    }
}

#[allow(non_camel_case_types)]
pub struct init {
    mutexed_cache: Mutex<LruCache<String, Result<Regex, String>>>,
}

const PRIV_ANCHOR: *const c_void = [0].as_ptr() as *const c_void;
const NAME: &CStr = c"rers";

pub struct Captures<'a> {
    caps: regex::bytes::Captures<'a>,
    #[allow(dead_code)]
    text: Option<Vec<u8>>,
    #[allow(dead_code)]
    slice: Option<&'a [u8]>,
}

// cheat: this is not exposed, but we know it exists
// Compiler bug: https://github.com/rust-lang/rust-clippy/pull/9948#discussion_r1821113636
// In the future Rust versions this `expect` should be removed
#[expect(improper_ctypes)]
extern "C" {
    pub fn THR_GetBusyobj() -> *mut ffi::busyobj;
    pub fn THR_GetRequest() -> *mut ffi::req;
}

#[derive(Default)]
struct Vxp {
    steps: Vec<(Regex, String)>,
    body: Vec<u8>,
    sent: Option<usize>,
}

impl Vxp {
    fn new(vrt_ctx: &Ctx) -> InitResult<Vxp> {
        unsafe {
            match ffi::VRT_priv_task_get(vrt_ctx.raw, PRIV_ANCHOR)
                .as_mut()
                .and_then(|p| (*p).take::<Vxp>())
            {
                None => InitResult::Pass,
                Some(p) => InitResult::Ok(*p),
            }
        }
    }
}

impl VDP for Vxp {
    fn name() -> &'static CStr {
        NAME
    }

    fn new(vrt_ctx: &mut Ctx, _: &mut VDPCtx) -> InitResult<Vxp> {
        // we don't know how/if the body will be modified, so we nuke the content-length
        // it's also not worth fleshing out a rust object just to remove a header, we just use the C functions
        unsafe {
            let req = vrt_ctx.raw.req.as_ref().unwrap();
            assert_eq!(req.magic, ffi::REQ_MAGIC);
            ffi::http_Unset((*vrt_ctx.raw.req).resp, ffi::H_Content_Length.as_ptr());
        }

        Vxp::new(vrt_ctx)
    }

    fn push(&mut self, ctx: &mut VDPCtx, act: VdpAction, buf: &[u8]) -> PushResult {
        self.body.extend_from_slice(buf);

        if !matches!(act, VdpAction::End) {
            return PushResult::Ok;
        }
        let mut replaced_body = Cow::from(&self.body);
        for (re, sub) in &self.steps {
            // ignore the `Cow::Borrowed` case, it means nothing changed
            if let Cow::Owned(s) = re.replace(&replaced_body, sub.as_bytes()) {
                replaced_body = Cow::from(s);
            }
        }
        ctx.push(act, &replaced_body)
    }
}

impl VFP for Vxp {
    fn name() -> &'static CStr {
        NAME
    }

    fn new(vrt_ctx: &mut Ctx, vdp_ctx: &mut VFPCtx) -> InitResult<Self> {
        unsafe {
            ffi::http_Unset(vdp_ctx.raw.resp, ffi::H_Content_Length.as_ptr());
        }

        Vxp::new(vrt_ctx)
    }

    fn pull(&mut self, ctx: &mut VFPCtx, buf: &mut [u8]) -> PullResult {
        // first pull everything, using buf to receive the initial data before extending our body vector
        while self.sent.is_none() {
            match ctx.pull(buf) {
                PullResult::Err => return PullResult::Err,
                PullResult::Ok(sz) => self.body.extend_from_slice(&buf[..sz]),
                PullResult::End(sz) => {
                    self.body.extend_from_slice(&buf[..sz]);
                    // same trick as for VDP, we run all our regex, but this time we'll revert the
                    // body back into a vector for the next times we are called
                    let mut replaced_body = Cow::from(&self.body);
                    for (re, sub) in &self.steps {
                        // ignore the `Cow::Borrowed` case, it means nothing changed
                        if let Cow::Owned(s) = re.replace(&replaced_body, sub.as_bytes()) {
                            replaced_body = Cow::from(s);
                        }
                    }
                    self.body = replaced_body.into_owned();
                    self.sent = Some(0);
                }
            }
        }
        // the body is completely in memory and fully transformed, we just need to copy whatever we
        // can into buf, and keep track of the data already transferred
        let mut out = self.sent.unwrap();
        assert!(out <= self.body.len());
        let len = std::cmp::min(buf.len(), self.body.len() - out);
        buf[..len].copy_from_slice(&self.body[out..(out + len)]);
        out += len;
        self.sent = Some(out);
        if out == self.body.len() {
            PullResult::End(len)
        } else {
            PullResult::Ok(len)
        }
    }
}

static PRIV_VXP_METHODS: vmod_priv_methods = vmod_priv_methods {
    magic: VMOD_PRIV_METHODS_MAGIC,
    type_: c"VXP type".as_ptr(),
    fini: Some(vmod_priv::on_fini::<Vxp>),
};
