pub mod js;
mod log;
mod modify;

pub use self::log::*;
use modify::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Reject,
    Redirect(String),
    ModifyRequest(Modify),
    ModifyResponse(Modify),
    LogRes,
    LogReq,
    Js(String),
}
