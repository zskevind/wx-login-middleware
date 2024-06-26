use crate::core::config::Config;
use crate::core::security::Authority;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{fmt::Display, sync::Arc};
use tiny_crypto::encoding::{Encoder, BASE64};

/// The error type of Result used in this crate.
pub use crate::core::security::Error;
/// The secret part such as keys in WxLoginInfo
pub use crate::core::security::ServerSession as Secret;

pub(crate) const LOGIN_FAIL_MSG: &str = "登录验证失败";
#[allow(dead_code)]
pub(crate) const AUTH_FAIL_MSG: &str = "登录会话验证失败";
pub(crate) const WX_JSCODE2SESSION_URL: &str = "https://api.weixin.qq.com/sns/jscode2session";

/// The login ok result.
#[derive(Serialize, Debug)]
pub struct WxLoginOk {
    pub openid: String,
    pub stoken: String,
    pub skey: String,
}

/// The login fail result.
#[derive(Serialize, Debug, Clone)]
pub struct WxLoginErr {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub detail: String,
}

/// The inner struct of [WxLoginInfo].
#[derive(Debug)]
pub struct WxLoginInfoInner {
    pub appid: String,
    pub openid: String,
    pub secret: Secret,
    pub sig_authed: bool,
}

/// The authentication result for login status.
#[derive(Debug, Clone)]
pub struct WxLoginInfo(Arc<WxLoginInfoInner>);
impl WxLoginInfo {
    pub fn new(inner: WxLoginInfoInner) -> Self {
        Self(Arc::new(inner))
    }
}
impl std::ops::Deref for WxLoginInfo {
    type Target = WxLoginInfoInner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The core struct for login and authentication process.
#[derive(Debug, Clone)]
pub struct WxLogin {
    pub cfg: Arc<Config>,
}

impl WxLogin {
    /// Create a new WxLogin with Config.
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }

    /// Handle login request.
    #[tracing::instrument(err(Debug), ret, skip_all)]
    pub async fn handle_login(&self, appid: String, code: String) -> Result<WxLoginOk, WxLoginErr> {
        tracing::info!("start handle_login({appid}, {code})");
        let app_info = self.cfg.app_map.get(&appid).ok_or(WxLoginErr {
            status: 401,
            code: "appid-not-found".into(),
            message: LOGIN_FAIL_MSG.into(),
            detail: "".into(),
        })?;
        let client = reqwest::Client::new();
        let url = WX_JSCODE2SESSION_URL;
        let code2sess_req =
            proto::Code2SessionRequest::from(appid.clone(), app_info.secret.0.clone(), code);
        let res = client
            .get(url)
            .query(&code2sess_req)
            .send()
            .await
            .map_err(err_resp(500, "jscode2session-call-fail"))?;
        let code2sess_res = res
            .json::<proto::Code2SessionResponse>()
            .await
            .map_err(err_resp(401, "jscode2session-resp-fail"))?;
        tracing::info!(?code2sess_res);
        let openid = code2sess_res.openid;
        let session_key: [u8; 16] = BASE64
            .from_text(&code2sess_res.session_key)
            .map_err(err_resp(500, "session-key-invalid-base64"))?
            .try_into()
            .map_err(|v: Vec<u8>| format!("unexpected key len: {}", v.len()))
            .map_err(err_resp(500, "session-key-invalid-base64"))?;
        let authority = Authority::new(app_info);
        let client_sess = authority.make_client_session(&openid, &session_key);
        Ok(WxLoginOk {
            openid: openid.clone(),
            stoken: ["ST1".into(), appid, openid, client_sess.sess_token].join(":"),
            skey: client_sess.sess_key,
        })
    }

    /// Authenticate login status.
    #[tracing::instrument(err, ret, skip(self))]
    pub fn authenticate(
        &self,
        stoken: &str,
        uri: &str,
        sig: Result<&str, Error>,
    ) -> Result<WxLoginInfo, Error> {
        let (tag, appid, openid, token_str) =
            stoken.split(":").next_tuple().ok_or("bad stoken format")?;
        if tag != "ST1" {
            return Err(format!("bad stoken tag:{tag}").into());
        }
        let app_info = self.cfg.app_map.get(appid).ok_or("appid not found")?;
        let authority = Authority::new(app_info);
        let secret = authority.auth_client_session(openid, token_str)?;
        let mut sig_authed = false;
        if self.cfg.auth_sig {
            let (tag, ts_ms_str, nonce_str, sig_str) =
                sig?.split(":").next_tuple().ok_or("bad sig format")?;
            if tag != "SG1" {
                return Err(format!("bad sig tag:{tag}").into());
            }
            authority.auth_client_sig(
                &BASE64.to_text(&secret.client_sess_key),
                uri,
                ts_ms_str,
                nonce_str,
                sig_str,
                |dur, _nonce| dur <= Duration::from_secs(self.cfg.sig_valid_secs),
            )?;
            sig_authed = true;
        }
        Ok(WxLoginInfo::new(WxLoginInfoInner {
            appid: appid.into(),
            openid: openid.into(),
            secret,
            sig_authed,
        }))
    }
}

fn err_resp<E: Display>(status: u16, code: &str) -> impl '_ + FnOnce(E) -> WxLoginErr {
    move |e| WxLoginErr {
        status,
        code: code.into(),
        message: LOGIN_FAIL_MSG.into(),
        detail: e.to_string(),
    }
}

mod proto {
    use super::*;

    #[derive(Serialize)]
    pub(crate) struct Code2SessionRequest {
        appid: String,
        secret: String,
        js_code: String,
        grant_type: String,
    }

    impl Code2SessionRequest {
        pub(crate) fn from(appid: String, secret: String, code: String) -> Self {
            Self {
                appid,
                secret,
                js_code: code,
                grant_type: "authorization_code".into(),
            }
        }
    }

    #[derive(Deserialize, Debug)]
    pub(crate) struct Code2SessionResponse {
        pub(crate) session_key: String,
        pub(crate) openid: String,
        pub(crate) _unionid: Option<String>,
    }
}
