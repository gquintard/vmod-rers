#![allow(non_camel_case_types)]

varnish::boilerplate!();

use std::sync::Mutex;
use varnish::vcl::convert::IntoVCL;
use varnish::vcl::ctx::Ctx;
use varnish::vcl::vpriv::VPriv;
use varnish_sys::VCL_STRING;

varnish::vtc!(test01);
varnish::vtc!(test02);
varnish::vtc!(test03);

pub struct store {
    mutexed_cache: Mutex<regex_cache::RegexCache>,
}

impl store {
    pub fn new(_ctx: &Ctx, _vcl_name: &str, opt_sz: Option<i64>) -> Self {
        let sz = match opt_sz {
            Some(n) if n > 0 => n,
            _ => 1000,
        };
        store {
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

    pub fn capture<'a>(
        &self,
        _: &mut Ctx,
        vp: &mut VPriv<regex::Captures<'a>>,
        s: &'a str,
        res: &str,
    ) -> bool {
        vp.clear();

        let re = match self.get_regex(res) {
            Err(_) => return false,
            Ok(re) => re.clone(),
        };

        let cap = match re.captures(s) {
            None => return false,
            Some(cap) => cap,
        };
        vp.store(cap);
        true
    }

    pub fn group<'a, 'b: 'a>(
        &self,
        _: &mut Ctx,
        vp: &mut VPriv<regex::Captures<'b>>,
        n: i64,
    ) -> &'a str {
        let n = if n >= 0 { n } else { 0 } as usize;
        vp.as_ref()
            .and_then(|cap| cap.get(n))
            .map(|m| m.as_str())
            .unwrap_or("")
    }
}
