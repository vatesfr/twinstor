//! XAPI's local storage layer (the SMAPI multiplexer), reached through the
//! local message switch with `message-cli`. Local only: it answers even when
//! the pool master does not (an SM deactivate still asks the master to drop
//! the host's activation key, which bounds it).
//!
//! Why twinstor needs it: a guest domain destroyed outside xenopsd (`xl
//! destroy` in the split-brain crash boundary, when xenopsd's own teardown
//! does not run or fails) leaves its datapaths (`vbd/<domid>/<dev>`)
//! "Activated" in the storage layer. The next start of that disk on this host
//! then skips SM's attach and activate and reuses the stale backend node,
//! which names a blktap minor that a newer tapdisk may own: disks wired to the
//! wrong VDI, or a start that cannot open its disk. `DP.destroy` is the one
//! clean release: SM deactivates and detaches the VDI as on a normal
//! shutdown (tapdisk closed, activation refcount dropped, LV deactivated,
//! the host's sm-config key removed). Killing the tapdisk by hand does none of
//! the bookkeeping.

/// The storage multiplexer's queue on the message switch.
pub const QUEUE: &str = "org.xen.xapi.storage";

/// Timeout handed to `message-cli` for one call, in seconds.
pub const CALL_TIMEOUT_SECS: u64 = 10;

/// A parsed JSON value: the storage API answers JSON-RPC. Just enough of
/// JSON for those answers; nothing here needs a full library.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Parse one JSON document (surrounding whitespace allowed). `None` on any
/// syntax error or trailing data.
pub fn parse_json(text: &str) -> Option<Json> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    (p.i == p.s.len()).then_some(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > 64 {
            return None;
        }
        self.ws();
        match *self.s.get(self.i)? {
            b'n' => self.eat("null").then_some(Json::Null),
            b't' => self.eat("true").then_some(Json::Bool(true)),
            b'f' => self.eat("false").then_some(Json::Bool(false)),
            b'"' => self.string().map(Json::Str),
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("]").then_some(Json::Arr(items));
                }
            }
            b'{' => {
                self.i += 1;
                let mut kv = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Some(Json::Obj(kv));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if !self.eat(":") {
                        return None;
                    }
                    kv.push((k, self.value(depth + 1)?));
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("}").then_some(Json::Obj(kv));
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                while self.i < self.s.len()
                    && matches!(
                        self.s[self.i],
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                    )
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Json::Num)
            }
            _ => None,
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat("\"") {
            return None;
        }
        let mut out = String::new();
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
                            self.i += 4;
                            out.push(
                                char::from_u32(u32::from_str_radix(hex, 16).ok()?)
                                    .unwrap_or('\u{fffd}'),
                            );
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Copy the whole UTF-8 sequence starting here.
                    let start = self.i - 1;
                    let len = match c {
                        0x00..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    let seq = std::str::from_utf8(self.s.get(start..start + len)?).ok()?;
                    out.push_str(seq);
                    self.i = start + len;
                }
            }
        }
    }
}

/// JSON string literal for `s`.
pub fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A named argument of a storage API call.
#[derive(Debug, Clone, Copy)]
pub enum Arg<'a> {
    Str(&'a str),
    Bool(bool),
    /// The `unit` argument some calls declare: it must be present, as null.
    Null,
}

/// The JSON-RPC body of one call: every argument is named, in the single
/// object the IDL expects as `params[0]`.
pub fn request_body(method: &str, args: &[(&str, Arg)]) -> String {
    let fields: Vec<String> = args
        .iter()
        .map(|(k, v)| {
            let v = match v {
                Arg::Str(s) => json_string(s),
                Arg::Bool(b) => b.to_string(),
                Arg::Null => "null".to_string(),
            };
            format!("{}:{v}", json_string(k))
        })
        .collect();
    format!(
        "{{\"method\":{},\"params\":[{{{}}}],\"id\":1}}",
        json_string(method),
        fields.join(",")
    )
}

/// The answer of one call: `Ok(result)` when `error` is null, `Err` with the
/// error flattened to text otherwise, or when the answer is not a JSON-RPC
/// response at all (never read as success).
pub fn parse_response(text: &str) -> Result<Json, String> {
    let v = parse_json(text.trim())
        .ok_or_else(|| format!("storage API answer not understood: {}", clip(text)))?;
    match v.get("error") {
        Some(Json::Null) => Ok(v.get("result").cloned().unwrap_or(Json::Null)),
        Some(e) => Err(flatten_error(e)),
        None => Err(format!(
            "storage API answer has no error field: {}",
            clip(text)
        )),
    }
}

/// Flattens an error such as
/// `["Backend_error",["SR_BACKEND_FAILURE_46",["","The VDI is not available",""]]]`
/// to `Backend_error: SR_BACKEND_FAILURE_46: The VDI is not available`.
fn flatten_error(e: &Json) -> String {
    fn walk(v: &Json, out: &mut Vec<String>) {
        match v {
            Json::Str(s) if !s.is_empty() => out.push(s.clone()),
            Json::Arr(items) => items.iter().for_each(|i| walk(i, out)),
            Json::Obj(kv) => kv.iter().for_each(|(_, i)| walk(i, out)),
            Json::Num(n) => out.push(n.to_string()),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    walk(e, &mut parts);
    if parts.is_empty() {
        "storage API error (empty)".to_string()
    } else {
        parts.join(": ")
    }
}

fn clip(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() > 200 {
        format!("{}…", t.chars().take(200).collect::<String>())
    } else {
        t.to_string()
    }
}

/// The datapaths `DP.stat_vdi` reports for one VDI, with their state
/// (`Activated`, `Attached`, ...). `None` when the result has no `dps`
/// object (an unknown shape must not read as "none").
pub fn parse_stat_vdi(result: &Json) -> Option<Vec<(String, String)>> {
    let Json::Obj(dps) = result.get("dps")? else {
        return None;
    };
    Some(
        dps.iter()
            .map(|(dp, st)| {
                let state = match st {
                    Json::Arr(items) => items
                        .first()
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string(),
                    Json::Str(s) => s.clone(),
                    _ => String::new(),
                };
                (dp.clone(), state)
            })
            .collect(),
    )
}

/// The domain a datapath belongs to: xenopsd names a guest disk's datapath
/// `vbd/<domid>/<device>`. `None` for any other name (never treated as
/// stale).
pub fn dp_domid(dp: &str) -> Option<u32> {
    let mut it = dp.split('/');
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some("vbd"), Some(d), Some(dev), None) if !dev.is_empty() => d.parse().ok(),
        _ => None,
    }
}

/// The VDIs `DP.diagnostics` lists under SR `sr`. `None` when the text is not
/// the diagnostics report this parser knows.
pub fn diagnostics_vdis(text: &str, sr: &str) -> Option<Vec<String>> {
    if !text.contains("The following SRs are attached:") {
        return None;
    }
    let mut cur_sr: Option<String> = None;
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("The following errors") || t.starts_with("No errors") {
            break;
        }
        if let Some(s) = t.strip_prefix("SR ") {
            cur_sr = Some(s.trim().to_ascii_lowercase());
        } else if let Some(v) = t.strip_prefix("VDI ")
            && cur_sr.as_deref() == Some(sr.to_ascii_lowercase().as_str())
        {
            let v = v
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if !v.is_empty() && !out.contains(&v) {
                out.push(v);
            }
        }
    }
    Some(out)
}

/// The exact command an operator runs to release one datapath by hand.
pub fn destroy_command(dp: &str) -> String {
    format!(
        "message-cli call {QUEUE} --timeout 60 --body '{}'",
        request_body(
            "DP.destroy",
            &[
                ("dbg", Arg::Str("operator")),
                ("dp", Arg::Str(dp)),
                ("allow_leak", Arg::Bool(false))
            ]
        )
    )
}

/// The exact command an operator runs to list the datapaths on this host.
pub fn diagnostics_command() -> String {
    format!(
        "message-cli call {QUEUE} --body '{}'",
        request_body(
            "DP.diagnostics",
            &[("dbg", Arg::Str("operator")), ("unit", Arg::Null)]
        )
    )
}

#[cfg_attr(test, allow(dead_code))]
async fn call(method: &str, args: &[(&str, Arg<'_>)]) -> Result<Json, String> {
    let body = request_body(method, args);
    let t = CALL_TIMEOUT_SECS.to_string();
    let (ok, stdout, stderr) = crate::cmd::cmd_status_for(
        "message-cli",
        &["call", QUEUE, "--timeout", &t, "--body", &body],
        CALL_TIMEOUT_SECS + 5,
    )
    .await
    .map_err(|e| format!("message-cli {method}: {e}"))?;
    match parse_response(&stdout) {
        Ok(v) => Ok(v),
        Err(e) if ok || !stdout.trim().is_empty() => Err(format!("{method}: {e}")),
        Err(_) => Err(format!("message-cli {method} failed: {}", clip(&stderr))),
    }
}

/// The datapaths the storage layer holds on one VDI of `sr`.
#[cfg_attr(test, allow(dead_code))]
pub async fn stat_vdi(sr: &str, vdi: &str) -> Result<Vec<(String, String)>, String> {
    let r = call(
        "DP.stat_vdi",
        &[
            ("dbg", Arg::Str("twinstor")),
            ("sr", Arg::Str(sr)),
            ("vdi", Arg::Str(vdi)),
            ("unit", Arg::Null),
        ],
    )
    .await?;
    parse_stat_vdi(&r).ok_or_else(|| format!("DP.stat_vdi answer not understood for VDI {vdi}"))
}

/// Release one datapath: SM deactivates and detaches its VDI on this host
/// unless another datapath still holds it.
#[cfg_attr(test, allow(dead_code))]
pub async fn destroy_dp(dp: &str) -> Result<(), String> {
    call(
        "DP.destroy",
        &[
            ("dbg", Arg::Str("twinstor")),
            ("dp", Arg::Str(dp)),
            ("allow_leak", Arg::Bool(false)),
        ],
    )
    .await
    .map(|_| ())
}

/// The storage layer's own report of every attached VDI and its datapaths.
#[cfg_attr(test, allow(dead_code))]
pub async fn diagnostics() -> Result<String, String> {
    match call(
        "DP.diagnostics",
        &[("dbg", Arg::Str("twinstor")), ("unit", Arg::Null)],
    )
    .await?
    {
        Json::Str(s) => Ok(s),
        other => Err(format!("DP.diagnostics answer not understood: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Answers captured from the XCP-ng 8.3 storage layer (xapi 26.1).
    const STAT: &str = r#"{"result":{"dps":{"vbd/3/xvdb":["Activated","RW"]},"superstate":["Activated","RW"]},"error":null,"id":0}"#;
    const STAT_TWO: &str = r#"{"result":{"dps":{"vbd/6/xvda":["Activated","RW"],"vbd/4/xvda":["Activated","RW"]},"superstate":["Activated","RW"]},"error":null,"id":0}"#;
    const DESTROY_OK: &str = r#"{"result":null,"error":null,"id":0}"#;
    const DESTROY_46: &str = r#"{"result":null,"error":["Backend_error",["SR_BACKEND_FAILURE_46",["","The VDI is not available [opterr=Command ['/usr/bin/vhd-util', 'query', '--debug', '-vsaf', '-n', '/dev/VG_XenStorage-5e5e000f-0000-4000-8000-00000000000f/VHD-5e5e000e-0000-4000-8000-00000000000e'] failed (2): error opening /dev/VG_XenStorage-5e5e000f-0000-4000-8000-00000000000f/VHD-5e5e000e-0000-4000-8000-00000000000e: -2]",""]]],"id":0}"#;
    const NO_UNIT: &str = r#"{"result":null,"error":["Internal_error","Idl.MarshalError(\"Expecting named argument 'unit'\")"],"id":0}"#;
    const SR: &str = "5e5e000f-0000-4000-8000-00000000000f";

    #[test]
    fn real_answers_parse() {
        let r = parse_response(STAT).unwrap();
        assert_eq!(
            parse_stat_vdi(&r).unwrap(),
            vec![("vbd/3/xvdb".to_string(), "Activated".to_string())]
        );
        let r = parse_response(STAT_TWO).unwrap();
        assert_eq!(parse_stat_vdi(&r).unwrap().len(), 2);
        assert_eq!(parse_response(DESTROY_OK).unwrap(), Json::Null);
        let e = parse_response(DESTROY_46).unwrap_err();
        assert!(
            e.starts_with("Backend_error: SR_BACKEND_FAILURE_46: The VDI is not available"),
            "{e}"
        );
        let e = parse_response(NO_UNIT).unwrap_err();
        assert!(e.contains("Expecting named argument 'unit'"), "{e}");
        // An empty dps map is a real "none"; a missing one is unknown.
        let r = parse_response(
            r#"{"result":{"dps":{},"superstate":["Detached","RO"]},"error":null,"id":0}"#,
        )
        .unwrap();
        assert_eq!(parse_stat_vdi(&r), Some(vec![]));
        assert_eq!(parse_stat_vdi(&Json::Null), None);
    }

    #[test]
    fn garbage_never_reads_as_success() {
        for t in [
            "",
            "timeout",
            "{\"result\":null}",
            "{\"error\":null",
            "[1,2]x",
            "message-switch: connection refused",
        ] {
            assert!(parse_response(t).is_err(), "{t:?}");
        }
    }

    #[test]
    fn json_round_trips_escapes_and_unicode() {
        let v = parse_json(r#" {"a":"x\"y\\zéé","b":[1,-2.5e3,true,false,null],"c":{}} "#).unwrap();
        assert_eq!(v.get("a").unwrap().as_str().unwrap(), "x\"y\\zéé");
        assert_eq!(
            v.get("b"),
            Some(&Json::Arr(vec![
                Json::Num(1.0),
                Json::Num(-2500.0),
                Json::Bool(true),
                Json::Bool(false),
                Json::Null
            ]))
        );
        assert_eq!(
            parse_json(&json_string("q\"\\\n\u{1}")).unwrap(),
            Json::Str("q\"\\\n\u{1}".into())
        );
        assert!(parse_json("{\"a\":1,}").is_none());
        assert!(parse_json(&"[".repeat(100)).is_none(), "nesting is bounded");
    }

    #[test]
    fn request_bodies_match_what_the_storage_layer_accepts() {
        assert_eq!(
            request_body(
                "DP.destroy",
                &[
                    ("dbg", Arg::Str("twinstor")),
                    ("dp", Arg::Str("vbd/3/xvda")),
                    ("allow_leak", Arg::Bool(false))
                ]
            ),
            r#"{"method":"DP.destroy","params":[{"dbg":"twinstor","dp":"vbd/3/xvda","allow_leak":false}],"id":1}"#
        );
        assert_eq!(
            request_body(
                "DP.diagnostics",
                &[("dbg", Arg::Str("x")), ("unit", Arg::Null)]
            ),
            r#"{"method":"DP.diagnostics","params":[{"dbg":"x","unit":null}],"id":1}"#
        );
        let c = destroy_command("vbd/3/xvda");
        assert!(c.starts_with("message-cli call org.xen.xapi.storage --timeout 60 --body '{\"method\":\"DP.destroy\""), "{c}");
        assert!(c.contains("\"dp\":\"vbd/3/xvda\""), "{c}");
        assert_eq!(
            diagnostics_command(),
            r#"message-cli call org.xen.xapi.storage --body '{"method":"DP.diagnostics","params":[{"dbg":"operator","unit":null}],"id":1}'"#
        );
    }

    #[test]
    fn only_guest_disk_datapaths_name_a_domain() {
        assert_eq!(dp_domid("vbd/3/xvda"), Some(3));
        assert_eq!(dp_domid("vbd/0/xvdb"), Some(0));
        for other in [
            "vbd/x/xvda",
            "vbd/3",
            "vbd/3/",
            "mirror_vbd/3/xvda",
            "vbd/3/xvda/extra",
            "sm-cli",
        ] {
            assert_eq!(dp_domid(other), None, "{other}");
        }
    }

    #[test]
    fn diagnostics_lists_the_vdis_of_one_sr() {
        // Shape of the report with a leaked datapath (device details cut).
        let text = format!(
            "The following SRs are attached:\n    SR 5e5e0011-0000-4000-8000-000000000011\n    SR {SR}\n        VDI 5e5e000e-0000-4000-8000-00000000000e\n            activated RW (device=Some {{}})\n                DP: vbd/3/xvda: activated RW  ** LEAKED\n        VDI 5e5e0002-0000-4000-8000-000000000002\n            activated RW (device=Some {{}})\n                DP: vbd/6/xvda: activated RW\n                DP: vbd/4/xvda: activated RW\n    SR 5e5e0003-0000-4000-8000-000000000003\n        VDI 11111111-2222-3333-4444-555555555555\n\nThe following errors have been logged:\nvbd/6/xvda @ 20261016T15:28:28Z; sr:{SR} vdi:deadbeef-0000-0000-0000-000000000000 error:x\n"
        );
        assert_eq!(
            diagnostics_vdis(&text, SR).unwrap(),
            vec![
                "5e5e000e-0000-4000-8000-00000000000e".to_string(),
                "5e5e0002-0000-4000-8000-000000000002".to_string()
            ]
        );
        let clean = format!(
            "The following SRs are attached:\n    SR {SR}\n\nNo errors have been logged.\n"
        );
        assert_eq!(diagnostics_vdis(&clean, SR), Some(vec![]));
        assert_eq!(diagnostics_vdis("something else", SR), None);
    }
}
