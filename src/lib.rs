varnish::boilerplate!();

use std::borrow::Cow;
use std::cmp::max;
use std::os::raw::c_void;
use std::slice;
use std::sync::Mutex;

use lru::LruCache;
use regex::bytes::Regex;

use varnish::vcl::convert::IntoVCL;
use varnish::vcl::ctx::{Ctx, Event, LogTag, TestCtx};
use varnish::vcl::processor::{
    new_vdp, new_vfp, InitResult, PullResult, PushAction, PushResult, VDPCtx, VFPCtx, VDP, VFP,
};
use varnish::vcl::vpriv::VPriv;
use varnish_sys::VCL_STRING;

varnish::vtc!(test01);
varnish::vtc!(test02);
varnish::vtc!(test03);
varnish::vtc!(test04);
varnish::vtc!(test05);
varnish::vtc!(test06);
varnish::vtc!(test07);

#[allow(non_camel_case_types)]
pub struct init {
    mutexed_cache: Mutex<LruCache<String, Result<Regex, String>>>,
}

const PRIV_ANCHOR: *const c_void = [0].as_ptr() as *const c_void;
const NAME: &str = "rers\0";

pub struct Captures<'a> {
    caps: regex::bytes::Captures<'a>,
    #[allow(dead_code)]
    text: Option<Vec<u8>>,
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
            }
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

        // we need rust to trust us on the lifetime of slice (which caps will
        // points to), so we go to raw parts and back again to trick it. It's not awesome, but it
        // works
        let ptr = body.as_ptr();
        let len = body.len();
        let slice = unsafe { slice::from_raw_parts(ptr, len) };
        match re.captures(slice) {
            None => Ok(false),
            Some(caps) => {
                vp.store(Captures {
                    caps,
                    text: Some(body),
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

    pub fn group<'a>(
        &self,
        _ctx: &mut Ctx,
        vp: &mut VPriv<Captures<'a>>,
        n: i64,
    ) -> Option<&'a [u8]> {
        let n = if n >= 0 { n } else { 0 } as usize;
        vp.as_ref()
            .and_then(|c| c.caps.get(n))
            .map(|m| m.as_bytes())
    }

    pub fn named_group<'a>(
        &self,
        _ctx: &mut Ctx,
        vp: &mut VPriv<Captures<'a>>,
        name: &str,
    ) -> Option<&'a [u8]> {
        vp.as_ref()
            .and_then(|c| c.caps.name(name))
            .map(|m| m.as_bytes())
    }

    pub fn replace_resp_body(&self, ctx: &mut Ctx, res: &str, sub: &str) {
        let re = match self.get_regex(res) {
            Err(s) => {
                ctx.log(LogTag::VclError, &s);
                return;
            }
            Ok(re) => re,
        };
        let priv_opt = unsafe { varnish_sys::VRT_priv_task(ctx.raw, PRIV_ANCHOR).as_mut() };
        if priv_opt.is_none() {
            ctx.fail("rers: couldn't retrieve priv_task (workspace too small?)");
            return;
        }
        let mut vp: VPriv<VXP> = VPriv::new(priv_opt.unwrap());
        if let Some(ri) = vp.as_mut() {
            ri.steps.push((re, sub.to_owned()));
        } else {
            let ri = VXP {
                body: Vec::new(),
                steps: vec![(re, sub.to_owned())],
                sent: None,
            };
            vp.store(ri);
        }
    }
}

// cheat: this is not exposed, but we know it exists
extern "C" {
    pub fn THR_GetBusyobj() -> *mut varnish_sys::busyobj;
    pub fn THR_GetRequest() -> *mut varnish_sys::req;
}

#[derive(Default)]
struct VXP {
    steps: Vec<(Regex, String)>,
    body: Vec<u8>,
    sent: Option<usize>,
}

impl VXP {
    fn new() -> InitResult<VXP> {
        let priv_opt;
        unsafe {
            // the lying! the cheating!
            let mut fake_ctx = TestCtx::new(0);
            fake_ctx.ctx().raw.req = THR_GetRequest();
            fake_ctx.ctx().raw.bo = THR_GetBusyobj();
            priv_opt = varnish_sys::VRT_priv_task_get(fake_ctx.ctx().raw, PRIV_ANCHOR)
                .as_mut()
                .and_then(|p| VPriv::new(p).take());
        }

        match priv_opt {
            None => InitResult::Pass,
            Some(p) => InitResult::Ok(p),
        }
    }
}

impl VDP for VXP {
    fn new(vrt_ctx: &Ctx, vdp_ctx: &mut VDPCtx, _oc: *mut varnish_sys::objcore) -> InitResult<VXP> {
        // we don't know how/if the body will be modified, so we nuke the content-length
        // it's also not worth fleshing out a rust object just to remove a header, we just use the C functions
        unsafe {
            let req = vdp_ctx.raw.req.as_ref().unwrap();
            assert_eq!(req.magic, varnish_sys::REQ_MAGIC);
            varnish_sys::http_Unset((*vdp_ctx.raw.req).resp, varnish_sys::H_Content_Length.as_ptr());
        }

        VXP::new()
    }

    fn push(&mut self, ctx: &mut VDPCtx, act: PushAction, buf: &[u8]) -> PushResult {
        self.body.extend_from_slice(buf);

        if !matches!(act, PushAction::End) {
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

    fn name() -> &'static str {
        NAME
    }
}

impl VFP for VXP {
    fn new(_vrt_ctx: &Ctx, vdp_ctx: &mut VFPCtx) -> InitResult<Self> {
        unsafe {
            varnish_sys::http_Unset(vdp_ctx.raw.resp, varnish_sys::H_Content_Length.as_ptr());
        }

        VXP::new()
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

    fn name() -> &'static str {
        NAME
    }
}

pub unsafe fn event(
    ctx: &mut Ctx,
    vp: &mut VPriv<(varnish_sys::vfp, varnish_sys::vdp)>,
    event: Event,
) -> Result<(), String> {
    match event {
        Event::Load => {
            vp.store((new_vfp::<VXP>(), new_vdp::<VXP>()));
            varnish_sys::VRT_AddFilter(ctx.raw, &vp.as_ref().unwrap().0, &vp.as_ref().unwrap().1);
        }
        Event::Discard => {
            varnish_sys::VRT_RemoveFilter(ctx.raw, &vp.as_ref().unwrap().0, &vp.as_ref().unwrap().1);
        }
        _ => (),
    }
    Ok(())
}
