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
    /// The phone's active SIMs (reply to `list_sims`).
    Sims {
        entries: Vec<crate::registry::SimInfo>,
    },
    /// The network's reply to a raw `mmi` request.
    MmiResult {
        code: String,
        success: bool,
        #[serde(default)]
        response: Option<String>,
    },
    /// The result of a `set_voicemail` request.
    VoicemailResult {
        enabled: bool,
        success: bool,
        #[serde(default)]
        response: Option<String>,
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
    /// Place an outbound call. `sim_sub_id` omitted ⇒ the phone's default calling SIM.
    Dial {
        number: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sim_sub_id: Option<i32>,
    },
    /// End a call leg. `call_id` omitted ⇒ the phone ends the active call.
    Hangup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<CallId>,
    },
    /// Decline a ringing call. `call_id` omitted ⇒ the phone rejects the ringing call.
    Reject {
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
    /// Request the list of active SIMs.
    ListSims {},
    /// Run a raw MMI / USSD code on a SIM (low-level); reply is an `mmi_result`.
    Mmi {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sim_sub_id: Option<i32>,
    },
    /// Enable/disable carrier voicemail (the device picks the platform mechanism); reply
    /// is a `voicemail_result`. `number` is an optional voicemail target some carriers need.
    SetVoicemail {
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sim_sub_id: Option<i32>,
    },
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
    /// Place a call from `device` to `number` (optionally on a specific SIM).
    #[serde(rename = "call.dial")]
    CallDial {
        device: String,
        number: String,
        #[serde(default)]
        sim_sub_id: Option<i32>,
    },
    /// Answer the ringing call on `device`.
    #[serde(rename = "call.pickup")]
    CallPickup { device: String },
    /// Hang up the active call on `device`.
    #[serde(rename = "call.hangup")]
    CallHangup { device: String },
    /// Decline the ringing call on `device`.
    #[serde(rename = "call.reject")]
    CallReject { device: String },
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
    #[serde(rename = "call.list")]
    CallList { device: String },
    /// List the active SIMs on `device`.
    #[serde(rename = "sims.list")]
    SimsList { device: String },
    /// Run a raw MMI / USSD code on `device` (low-level escape hatch).
    #[serde(rename = "mmi.send")]
    Mmi {
        device: String,
        code: String,
        #[serde(default)]
        sim_sub_id: Option<i32>,
    },
    /// Enable/disable carrier voicemail on `device` (optionally a specific SIM/number).
    #[serde(rename = "voicemail.set")]
    VoicemailSet {
        device: String,
        enabled: bool,
        #[serde(default)]
        number: Option<String>,
        #[serde(default)]
        sim_sub_id: Option<i32>,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CallRecord;

    #[test]
    fn call_list_op_tag() {
        // The control op for the call log is `call.list` (grouped with call.*).
        let req = ControlRequest {
            id: "1".into(),
            op: ControlOp::CallList {
                device: "phone1".into(),
            },
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["op"], "call.list");
        assert_eq!(v["device"], "phone1");
    }

    #[test]
    fn call_reject_op_tag() {
        let req = ControlRequest {
            id: "1".into(),
            op: ControlOp::CallReject {
                device: "phone1".into(),
            },
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["op"], "call.reject");
        assert_eq!(v["device"], "phone1");
    }

    #[test]
    fn reject_action_flattens_into_cmd() {
        let cmd = ServerToPhone::Cmd {
            cmd_id: "c1".into(),
            action: Action::Reject { call_id: None },
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(v["type"], "cmd");
        assert_eq!(v["action"], "reject");
        // call_id omitted when None.
        assert!(v.get("call_id").is_none());
    }

    #[test]
    fn list_calls_action_flattens_into_cmd() {
        let cmd = ServerToPhone::Cmd {
            cmd_id: "c1".into(),
            action: Action::ListCalls {},
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(v["type"], "cmd");
        assert_eq!(v["cmd_id"], "c1");
        assert_eq!(v["action"], "list_calls");
    }

    #[test]
    fn calls_frame_parses_with_missing_number() {
        // A missed call with no number key must still deserialize (number -> None).
        let frame = r#"{"type":"calls","entries":[
            {"number":"+15551234","kind":"outgoing","ts":1000,"duration":7},
            {"kind":"missed","ts":2000,"duration":0}
        ]}"#;
        let msg: PhoneToServer = serde_json::from_str(frame).unwrap();
        match msg {
            PhoneToServer::Calls { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].number.as_deref(), Some("+15551234"));
                assert_eq!(entries[0].kind, "outgoing");
                assert_eq!(entries[1].number, None);
                assert_eq!(entries[1].kind, "missed");
            }
            other => panic!("expected Calls, got {other:?}"),
        }
    }

    #[test]
    fn sims_list_op_and_action() {
        let req = ControlRequest {
            id: "1".into(),
            op: ControlOp::SimsList {
                device: "phone1".into(),
            },
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["op"], "sims.list");

        let cmd = ServerToPhone::Cmd {
            cmd_id: "c1".into(),
            action: Action::ListSims {},
        };
        let cv: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(cv["action"], "list_sims");
    }

    #[test]
    fn sims_frame_parses() {
        let frame = r#"{"type":"sims","entries":[
            {"slot":0,"sub_id":1,"name":"SIM 1","carrier":"Carrier","number":"+1555"},
            {"slot":1,"sub_id":2}
        ]}"#;
        let msg: PhoneToServer = serde_json::from_str(frame).unwrap();
        match msg {
            PhoneToServer::Sims { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].slot, 0);
                assert_eq!(entries[0].number.as_deref(), Some("+1555"));
                assert_eq!(entries[1].sub_id, 2);
                assert_eq!(entries[1].name, None);
                assert_eq!(entries[1].number, None);
            }
            other => panic!("expected Sims, got {other:?}"),
        }
    }

    #[test]
    fn mmi_op_and_result() {
        let req = ControlRequest {
            id: "1".into(),
            op: ControlOp::Mmi {
                device: "phone1".into(),
                code: "##002#".into(),
                sim_sub_id: Some(9),
            },
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["op"], "mmi.send");
        assert_eq!(v["code"], "##002#");

        let frame = r###"{"type":"mmi_result","code":"##002#","success":true,"response":"Erased"}"###;
        match serde_json::from_str::<PhoneToServer>(frame).unwrap() {
            PhoneToServer::MmiResult { code, success, .. } => {
                assert_eq!(code, "##002#");
                assert!(success);
            }
            other => panic!("expected MmiResult, got {other:?}"),
        }
    }

    #[test]
    fn voicemail_op_action_and_result() {
        let req = ControlRequest {
            id: "1".into(),
            op: ControlOp::VoicemailSet {
                device: "phone1".into(),
                enabled: false,
                number: None,
                sim_sub_id: Some(9),
            },
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["op"], "voicemail.set");
        assert_eq!(v["enabled"], false);
        assert_eq!(v["sim_sub_id"], 9);

        let cmd = ServerToPhone::Cmd {
            cmd_id: "c1".into(),
            action: Action::SetVoicemail {
                enabled: true,
                number: Some("+15550000".into()),
                sim_sub_id: None,
            },
        };
        let cv: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(cv["action"], "set_voicemail");
        assert_eq!(cv["enabled"], true);
        assert_eq!(cv["number"], "+15550000");
        assert!(cv.get("sim_sub_id").is_none());

        let frame = r#"{"type":"voicemail_result","enabled":false,"success":true,"response":"Service has been disabled."}"#;
        match serde_json::from_str::<PhoneToServer>(frame).unwrap() {
            PhoneToServer::VoicemailResult { enabled, success, response } => {
                assert!(!enabled);
                assert!(success);
                assert_eq!(response.as_deref(), Some("Service has been disabled."));
            }
            other => panic!("expected VoicemailResult, got {other:?}"),
        }
    }

    #[test]
    fn call_record_roundtrip() {
        let rec = CallRecord {
            number: Some("+1".into()),
            kind: "incoming".into(),
            ts: 42,
            duration: 3,
        };
        let back: CallRecord = serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back.number, rec.number);
        assert_eq!(back.kind, rec.kind);
        assert_eq!(back.ts, rec.ts);
        assert_eq!(back.duration, rec.duration);
    }
}
