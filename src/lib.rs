use std::borrow::Cow;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::sync::Mutex;

use lru::LruCache;
use regex::bytes::Regex;
use varnish::ffi::{self, vmod_priv, vmod_priv_methods, VdpAction, VMOD_PRIV_METHODS_MAGIC};
use varnish::run_vtc_tests;
use varnish::vcl::{
    Ctx, DeliveryProcCtx, DeliveryProcessor, FetchProcCtx, FetchProcessor, InitResult, PullResult,
    PushResult,
};

run_vtc_tests!("tests/*.vtc");

/// General note: all functions in this vmod will silently fail if given an invalid
/// regex, which means that `.is_match()` and `.capture()` will always return false,
/// and replace will be a noop.
#[varnish::vmod(docs = "API.md")]
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
        /// Build a regex store, optionally specifying its size `n` (defaults to 100). The
        /// cache is a standard LRU cache, meaning that if we try to compile/access a regex
        /// that wouldn't fit in it, it will remove the Least Recently Used regex to make
        /// space for the newcomer.
        pub fn new(#[default(1000)] cache_size: i64) -> Self {
            let cache_size = max(0, cache_size) as usize;
            init {
                mutexed_cache: Mutex::new(LruCache::new(cache_size)),
            }
        }

        /// Return `true` if `regex` matches on `s`
        pub fn is_match(&self, s: &str, res: &str) -> bool {
            self.get_regex(res)
                .map(|re| re.is_match(s.as_bytes()))
                .unwrap_or(false)
        }

        /// Replace all groups matching `regex` in `s` with `sub`. If `lim` is specified,
        /// only the first `lim` groups are replaced.
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

        /// Equivalent to `is_match()`, but remembers the captured groups so you can access
        /// them with `group()` later on.
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

        /// Same as `.capture()` but works on the request body. The request must have been
        /// cached first (using `std.cache_req_body()` for example) or the call will fail
        /// and interrupt the VCL transaction. If the request body isn't valid utf8, the
        /// function will simply return `false`.
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

        /// Return a captured group (from `capture()` or `capture_req_body()`) using its
        /// `index` or its `name`. Trying to access an non-existing group will return an
        /// empty string.
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

        /// Return a captured (named) group (from `capture()` or `capture_req_body()`) using its
        /// `index` or its `name`. Trying to access an non-existing group will return an
        /// empty string.
        pub fn named_group<'a>(
            &self,
            #[shared_per_task] vp: &mut Option<Box<Captures<'a>>>,
            name: &str,
        ) -> Option<&'a [u8]> {
            vp.as_ref()
                .and_then(|c| c.caps.name(name))
                .map(|m| m.as_bytes())
        }

        /// Add a regex/substitute pair to use when delivering the response body to a
        /// client. Note that you will need to include `rers` in `resp.filters` for it to
        /// have an effect. This function can be called multiple times, with each pair being
        /// called sequentially.
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

impl DeliveryProcessor for Vxp {
    fn name() -> &'static CStr {
        NAME
    }

    fn new(vrt_ctx: &mut Ctx, _vdp_ctx: &mut DeliveryProcCtx) -> InitResult<Vxp> {
        // we don't know how/if the body will be modified, so we nuke the content-length
        let resp = vrt_ctx.http_resp.as_mut().or_else(|| vrt_ctx.http_beresp.as_mut()).unwrap();
        resp.unset_header("Content-Length");

        Vxp::new(vrt_ctx)
    }

    fn push(&mut self, ctx: &mut DeliveryProcCtx, act: VdpAction, buf: &[u8]) -> PushResult {
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

impl FetchProcessor for Vxp {
    fn name() -> &'static CStr {
        NAME
    }

    fn new(vrt_ctx: &mut Ctx, _: &mut FetchProcCtx) -> InitResult<Self> {
        // we don't know how/if the body will be modified, so we nuke the content-length
        let resp = vrt_ctx.http_resp.as_mut().or_else(|| vrt_ctx.http_beresp.as_mut()).unwrap();
        resp.unset_header("Content-Length");

        Vxp::new(vrt_ctx)
    }

    fn pull(&mut self, ctx: &mut FetchProcCtx, buf: &mut [u8]) -> PullResult {
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
