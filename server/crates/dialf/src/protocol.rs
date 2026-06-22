//! Wire types for both planes.
//!
//! Phone ↔ dialfd: JSON over WebSocket. The phone's first frame MUST be
//! [`PhoneToServer::Hello`] carrying the shared key, or the socket is closed.
//!
//! dialf/other tools ↔ dialfd: line-delimited JSON over a Unix domain socket
//! ([`ControlRequest`] / [`ControlResponse`]).

use serde::{Deserialize, Serialize};

/// Identifier for a phone-side call leg.
pub type CallId = String;
/// Correlates a [`ServerToPhone::Cmd`] with its [`PhoneToServer::Ack`].
pub type CmdId = String;

/// Call direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[serde(rename = "in")]
    In,
    #[serde(rename = "out")]
    Out,
}

/// Lifecycle state of a call leg, as reported by the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Ringing,
    Active,
    Ended,
}

// ---------------------------------------------------------------------------
// Phone -> dialfd
// ---------------------------------------------------------------------------

/// Messages a phone sends to `dialfd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PhoneToServer {
    /// Handshake; must be the first frame.
    Hello {
        device_id: String,
        name: String,
        key: String,
        #[serde(default)]
        caps: Vec<String>,
        #[serde(default)]
        app_version: Option<String>,
    },
    /// Liveness ping, ~every 30s.
    Heartbeat {
        ts: i64,
        #[serde(default)]
        battery: Option<u8>,
    },
    /// A call leg changed state.
    CallState {
        call_id: CallId,
        state: CallState,
        #[serde(default)]
        number: Option<String>,
        direction: Direction,
    },
    /// An SMS was received (or send confirmation echoed).
    Sms {
        direction: Direction,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
        body: String,
        ts: i64,
    },
    /// Snapshot of the phone's call log (reply to `list_calls`).
    Calls {
        entries: Vec<crate::registry::CallRecord>,
    },
    /// Acknowledges a [`ServerToPhone::Cmd`].
    Ack { cmd_id: CmdId, ok: bool },
    /// Reports a failure, optionally tied to a command.
    Error {
        #[serde(default)]
        cmd_id: Option<CmdId>,
        msg: String,
    },
}

// ---------------------------------------------------------------------------
// dialfd -> Phone
// ---------------------------------------------------------------------------

/// Messages `dialfd` sends to a phone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToPhone {
    /// A command for the phone to execute.
    Cmd {
        cmd_id: CmdId,
        #[serde(flatten)]
        action: Action,
    },
}

/// The concrete action carried by a [`ServerToPhone::Cmd`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Answer a ringing call. `call_id` omitted ⇒ the phone answers the ringing call.
    Pickup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<CallId>,
    },
    /// Place an outbound call.
    Dial { number: String },
    /// End a call leg. `call_id` omitted ⇒ the phone ends the active call.
    Hangup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<CallId>,
    },
    /// Send a text message.
    SendSms { to: String, body: String },
    /// Request the inbox (optionally since a timestamp).
    ListSms {
        #[serde(default)]
        since: Option<i64>,
    },
    /// Request the call log.
    ListCalls {},
    /// Replace the phone's local auto-pickup number list.
    SetAutopickup { numbers: Vec<String> },
}

// ---------------------------------------------------------------------------
// Control API (dialf / other tools -> dialfd)
// ---------------------------------------------------------------------------

/// A request on the local control socket. `id` is echoed back for correlation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub id: String,
    #[serde(flatten)]
    pub op: ControlOp,
}

/// Control operations available to `dialf` and other local tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlOp {
    /// List connected devices.
    #[serde(rename = "devices.list")]
    DevicesList,
    /// Place a call from `device` to `number`.
    #[serde(rename = "call.dial")]
    CallDial { device: String, number: String },
    /// Answer the ringing call on `device`.
    #[serde(rename = "call.pickup")]
    CallPickup { device: String },
    /// Hang up the active call on `device`.
    #[serde(rename = "call.hangup")]
    CallHangup { device: String },
    /// Send an SMS from `device`.
    #[serde(rename = "sms.send")]
    SmsSend {
        device: String,
        to: String,
        body: String,
    },
    /// List recent SMS on `device`.
    #[serde(rename = "sms.list")]
    SmsList { device: String },
    /// List the recent call log on `device`.
    #[serde(rename = "calls.list")]
    CallsList { device: String },
    /// Play an audio file out the sound card (optionally tied to a device's call).
    #[serde(rename = "audio.play")]
    AudioPlay {
        file: String,
        #[serde(default)]
        device: Option<String>,
    },
    /// Run a YAML job — either an inline step list or a path to load.
    #[serde(rename = "job.run")]
    JobRun {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        steps: Option<Vec<crate::jobs::schema::Step>>,
        #[serde(default)]
        device: Option<String>,
    },
    /// Query a running job's status.
    #[serde(rename = "job.status")]
    JobStatus { job_id: String },
}

/// A response (or streamed event) on the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    /// Echoes [`ControlRequest::id`].
    pub id: String,
    /// `true` for the terminal frame of a request; `false` for interim stream events.
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Arbitrary payload (device list, job event, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
