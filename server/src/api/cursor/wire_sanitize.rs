//! Best-effort repair of AgentClientMessage wire bytes when Cursor's
//! ConversationStateStructure drifts past our prost schema.
//!
//! Known failure modes (managed-local / long sessions):
//! - `plan` (field 7) / `summary` (field 6) arrive as fixed64/varint instead of LD
//! - `conversation_state` is a raw UUID ascii string (summary then reads as Varint)
//! - trailing garbage after valid CSS fields
//!
//! Strategy: drop mismatched CSS fields (incl. unexpected fixed64), truncate on
//! skip failure, replace UUID-as-CSS with empty CSS. If sanitize still cannot
//! decode, callers may fall back to [`clear_conversation_state`].

use std::{
    fs,
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const WT_VARINT: u8 = 0;
const WT_64: u8 = 1;
const WT_LD: u8 = 2;
const WT_32: u8 = 5;

/// Length-delimited CSS fields (bytes / string / message / map entry).
/// Includes forward-compat tags seen in newer Cursor extractions.
const CSS_LD: &[u32] = &[
    1, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 16, 18, 19, 20, 21, 22, 23, 24, 25, 27, 28, 29,
    30, 31, 32, 34, 35, 36, 38, 40, 42, 43, 44, 45, 49, 50,
];

/// Varint CSS fields (enums / uint / bool).
const CSS_VARINT: &[u32] = &[10, 17, 26, 33, 37, 39, 46, 47];

const ACM_RUN_REQUEST: u32 = 1;
const ARR_CONVERSATION_STATE: u32 = 1;
const ARR_ACTION: u32 = 2;
const ARR_MODEL_DETAILS: u32 = 3;

/// `aiserver.v1.InferenceService/RunInference` shares `BidiService/BidiAppend`
/// with `agent.v1.AgentService/Run`. Its `RunInferenceClientMessage.run_request`
/// (field 1) is a `RunInferenceRunRequest` whose field 1 is the conversation_id
/// string, so read as an ACM it looks like `conversation_state` = UUID ascii.
/// Cursor 3.27 sends these for client-driven agent turns; BYOK does not
/// implement RunInference, so these appends must not be decoded as ACM.
pub fn is_run_inference_client_message(payload: &[u8]) -> bool {
    let Some(run) = extract_ld(payload, ACM_RUN_REQUEST) else {
        return false;
    };
    let Some(first) = extract_ld(&run, 1) else {
        return false;
    };
    looks_like_uuid_ascii(&first) && extract_ld(&run, ARR_MODEL_DETAILS).is_some()
}

#[derive(Debug, Default, Clone)]
pub struct SanitizeNotes {
    pub lines: Vec<String>,
}

impl SanitizeNotes {
    pub fn push(&mut self, s: impl Into<String>) {
        self.lines.push(s.into());
    }
}

fn read_varint(buf: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut x: u64 = 0;
    let mut s: u32 = 0;
    while i < buf.len() {
        let b = buf[i];
        i += 1;
        x |= u64::from(b & 0x7f) << s;
        if b & 0x80 == 0 {
            return Some((x, i));
        }
        s += 7;
        if s > 63 {
            return None;
        }
    }
    None
}

fn encode_varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while v > 0x7f {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    out
}

fn skip_value(buf: &[u8], i: usize, wt: u8) -> Option<usize> {
    match wt {
        WT_VARINT => read_varint(buf, i).map(|(_, j)| j),
        WT_64 => (i + 8 <= buf.len()).then_some(i + 8),
        WT_32 => (i + 4 <= buf.len()).then_some(i + 4),
        WT_LD => {
            let (ln, j) = read_varint(buf, i)?;
            let end = j.checked_add(ln as usize)?;
            (end <= buf.len()).then_some(end)
        }
        _ => None,
    }
}

fn ld_field(num: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = encode_varint(u64::from((num << 3) | u32::from(WT_LD)));
    out.extend(encode_varint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

fn looks_like_uuid_ascii(buf: &[u8]) -> bool {
    if buf.len() != 36 {
        return false;
    }
    let Ok(s) = std::str::from_utf8(buf) else {
        return false;
    };
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty() && p.bytes().all(|c| c.is_ascii_hexdigit())
    })
}

fn extract_ld(buf: &[u8], field_num: u32) -> Option<Vec<u8>> {
    let mut i = 0usize;
    while i < buf.len() {
        let (tag, j) = read_varint(buf, i)?;
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u8;
        let end = skip_value(buf, j, wt)?;
        if field == field_num && wt == WT_LD {
            let (ln, k) = read_varint(buf, j)?;
            return Some(buf[k..k + ln as usize].to_vec());
        }
        i = end;
    }
    None
}

fn replace_ld(buf: &[u8], field_num: u32, new_msg: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0usize;
    let mut replaced = false;
    while i < buf.len() {
        let start = i;
        let (tag, j) = read_varint(buf, i)?;
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u8;
        let end = skip_value(buf, j, wt)?;
        if field == field_num && wt == WT_LD {
            out.extend(ld_field(field_num, new_msg));
            replaced = true;
        } else {
            out.extend_from_slice(&buf[start..end]);
        }
        i = end;
    }
    replaced.then_some(out)
}

fn upsert_ld(buf: &[u8], field_num: u32, new_msg: &[u8]) -> Option<Vec<u8>> {
    if let Some(r) = replace_ld(buf, field_num, new_msg) {
        return Some(r);
    }
    let mut out = buf.to_vec();
    out.extend(ld_field(field_num, new_msg));
    Some(out)
}

fn rebuild_with_css(payload: &[u8], clean_css: &[u8]) -> Option<Vec<u8>> {
    let run = extract_ld(payload, ACM_RUN_REQUEST)?;
    let new_run = upsert_ld(&run, ARR_CONVERSATION_STATE, clean_css)?;
    replace_ld(payload, ACM_RUN_REQUEST, &new_run)
}

fn sanitize_css(buf: &[u8], notes: &mut SanitizeNotes) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0usize;
    while i < buf.len() {
        let start = i;
        let Some((tag, j)) = read_varint(buf, i) else {
            notes.push(format!("truncate CSS: bad varint @ {start}"));
            break;
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u8;
        let Some(end) = skip_value(buf, j, wt) else {
            notes.push(format!(
                "truncate CSS: cannot skip field {field} wire={wt} @ {start}"
            ));
            break;
        };
        if CSS_LD.contains(&field) && wt != WT_LD {
            notes.push(format!(
                "drop CSS field {field} wire={wt} expected=2 @ {start}"
            ));
            i = end;
            continue;
        }
        if CSS_VARINT.contains(&field) && wt != WT_VARINT {
            notes.push(format!(
                "drop CSS field {field} wire={wt} expected=0 @ {start}"
            ));
            i = end;
            continue;
        }
        if wt == WT_64 {
            notes.push(format!(
                "drop CSS field {field} unexpected fixed64 @ {start}"
            ));
            i = end;
            continue;
        }
        out.extend_from_slice(&buf[start..end]);
        i = end;
    }
    out
}

/// Sanitize ACM bytes. Returns repaired payload + notes, or None if structure
/// is not an ACM.run_request we can walk.
pub fn sanitize_agent_client_message(payload: &[u8]) -> Option<(Vec<u8>, SanitizeNotes)> {
    if is_run_inference_client_message(payload) {
        return None;
    }
    let mut notes = SanitizeNotes::default();
    let run = extract_ld(payload, ACM_RUN_REQUEST)?;
    let css = match extract_ld(&run, ARR_CONVERSATION_STATE) {
        Some(c) => c,
        None => {
            notes.push("ARR missing conversation_state; inserting empty CSS");
            let repaired = rebuild_with_css(payload, &[])?;
            return Some((repaired, notes));
        }
    };
    notes.push(format!("css_len={}", css.len()));
    if looks_like_uuid_ascii(&css) {
        let uuid = String::from_utf8_lossy(&css);
        notes.push(format!("UUID ascii {uuid}; replace empty CSS"));
        let repaired = rebuild_with_css(payload, &[])?;
        return Some((repaired, notes));
    }
    let clean = sanitize_css(&css, &mut notes);
    notes.push(format!("css sanitized {} -> {}", css.len(), clean.len()));
    let repaired = rebuild_with_css(payload, &clean)?;
    Some((repaired, notes))
}

/// Force `AgentRunRequest.conversation_state` to empty CSS (last-resort).
pub fn clear_conversation_state(payload: &[u8]) -> Option<(Vec<u8>, SanitizeNotes)> {
    if is_run_inference_client_message(payload) {
        return None;
    }
    let mut notes = SanitizeNotes::default();
    notes.push("clear_conversation_state: empty CSS fallback");
    let repaired = rebuild_with_css(payload, &[])?;
    Some((repaired, notes))
}

/// Drop AgentRunRequest top-level fields that individually fail to decode.
///
/// Newer Cursor builds reuse ARR tags with different types (e.g. field 7 as
/// repeated model entries instead of `SkillOptions`, field 17/18/25 as varints).
/// prost rejects the whole message on such drift, so after CSS repair we test
/// each ARR field in isolation (wrapped in an ACM) with `decodes` and keep only
/// fields that parse. Field 1 (conversation_state) is always kept.
pub fn prune_run_request_fields(
    payload: &[u8],
    decodes: impl Fn(&[u8]) -> bool,
) -> Option<(Vec<u8>, SanitizeNotes)> {
    if is_run_inference_client_message(payload) {
        return None;
    }
    let mut notes = SanitizeNotes::default();
    let run = extract_ld(payload, ACM_RUN_REQUEST)?;
    let mut kept = Vec::with_capacity(run.len());
    let mut i = 0usize;
    while i < run.len() {
        let start = i;
        let Some((tag, j)) = read_varint(&run, i) else {
            notes.push(format!("truncate ARR: bad varint @ {start}"));
            break;
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u8;
        let Some(end) = skip_value(&run, j, wt) else {
            notes.push(format!("truncate ARR: cannot skip field {field} wire={wt} @ {start}"));
            break;
        };
        let chunk = &run[start..end];
        if field == ARR_ACTION && wt == WT_LD && !decodes(&ld_field(ACM_RUN_REQUEST, chunk)) {
            // Never silently drop the user's turn: without the action the run
            // has no user message and providers see an empty/prefill request.
            notes.push(format!("ARR action (field 2) does not decode @ {start}; refusing to prune"));
            return None;
        }
        if field == ARR_CONVERSATION_STATE || decodes(&ld_field(ACM_RUN_REQUEST, chunk)) {
            kept.extend_from_slice(chunk);
        } else {
            notes.push(format!("drop ARR field {field} wire={wt} len={} @ {start}", end - start));
        }
        i = end;
    }
    let repaired = replace_ld(payload, ACM_RUN_REQUEST, &kept)?;
    Some((repaired, notes))
}

fn wire_dump_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".cursor-byok-v3").join("logs").join("wire-dumps");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Best-effort dump of raw / repaired ACM hex for offline diagnosis.
pub fn dump_wire(tag: &str, raw: &[u8], repaired: Option<&[u8]>, notes: &SanitizeNotes) {
    let Some(dir) = wire_dump_dir() else {
        return;
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{ts}-{tag}.txt"));
    let mut body = String::new();
    body.push_str(&format!("tag={tag}\n"));
    body.push_str(&format!("raw_len={}\n", raw.len()));
    body.push_str(&format!("raw_hex={}\n", hex::encode(raw)));
    if let Some(r) = repaired {
        body.push_str(&format!("repaired_len={}\n", r.len()));
        body.push_str(&format!("repaired_hex={}\n", hex::encode(r)));
    }
    for line in &notes.lines {
        body.push_str("note=");
        body.push_str(line);
        body.push('\n');
    }
    if let Ok(mut f) = fs::File::create(&path) {
        let _ = f.write_all(body.as_bytes());
        tracing::warn!(path = %path.display(), tag, "wrote ACM wire dump");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn encode_varint_u32(v: u32) -> Vec<u8> {
        encode_varint(u64::from(v))
    }

    fn make_acm_with_css(css: &[u8]) -> Vec<u8> {
        // Minimal ARR: field1=css, field3=model_details(model_id only), field5=conversation_id
        let model = ld_field(1, b"toknex-test");
        let mut arr = ld_field(1, css);
        arr.extend(ld_field(3, &model));
        arr.extend(ld_field(5, b"conv-offline-1"));
        ld_field(1, &arr)
    }

    #[test]
    fn plan_fixed64_is_dropped_and_decodes() {
        let mut css = ld_field(8, b"");
        css.extend(encode_varint_u32((7 << 3) | u32::from(WT_64)));
        css.extend_from_slice(&0x1111u64.to_le_bytes());
        let payload = make_acm_with_css(&css);

        // Raw should fail prost decode on ConversationStateStructure.plan
        let raw_fail = crate::cursor::protocol::proto::agent::v1::AgentClientMessage::decode(
            payload.as_slice(),
        );
        assert!(raw_fail.is_err(), "expected raw plan fixed64 to fail");

        let (repaired, notes) = sanitize_agent_client_message(&payload).expect("sanitize");
        assert!(
            notes.lines.iter().any(|l| l.contains("drop CSS field 7")),
            "notes={:?}",
            notes.lines
        );
        let msg = crate::cursor::protocol::proto::agent::v1::AgentClientMessage::decode(
            repaired.as_slice(),
        )
        .expect("decode after sanitize");
        assert!(msg.message.is_some());
    }

    #[test]
    fn uuid_as_css_is_run_inference_not_repaired() {
        // A "UUID in conversation_state" ACM is really a RunInferenceRunRequest
        // (conversation_id = field 1); it must not be rewritten into an ACM.
        let css = b"06bd441c-e7b9-4d5b-9c51-3d8b712027c3";
        let payload = make_acm_with_css(css);
        assert!(is_run_inference_client_message(&payload));
        assert!(sanitize_agent_client_message(&payload).is_none());
    }

    #[test]
    fn clean_baseline_unchanged_and_decodes() {
        let mut css = ld_field(8, b"\x0a\x00");
        css.extend(ld_field(7, b"{\"steps\":[]}"));
        let payload = make_acm_with_css(&css);
        let (repaired, _notes) = sanitize_agent_client_message(&payload).expect("sanitize");
        // CSS should still contain plan LD
        let run = extract_ld(&repaired, 1).unwrap();
        let css2 = extract_ld(&run, 1).unwrap();
        assert!(css2.windows(ld_field(7, b"{\"steps\":[]}").len()).any(|w| w == ld_field(7, b"{\"steps\":[]}")));
        crate::cursor::protocol::proto::agent::v1::AgentClientMessage::decode(repaired.as_slice())
            .expect("clean decode");
    }

    #[test]
    fn truncate_garbage_tail() {
        let mut css = ld_field(8, b"ok");
        css.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        let payload = make_acm_with_css(&css);
        let (repaired, notes) = sanitize_agent_client_message(&payload).expect("sanitize");
        assert!(notes.lines.iter().any(|l| l.contains("truncate")));
        let run = extract_ld(&repaired, 1).unwrap();
        let css2 = extract_ld(&run, 1).unwrap();
        assert_eq!(css2, ld_field(8, b"ok"));
    }

    #[test]
    fn clear_conversation_state_fallback() {
        // summary (field 6) arriving as fixed32: 0x35 + 4 bytes
        let css: &[u8] = &[0x35, 0x01, 0x02, 0x03, 0x04];
        let payload = make_acm_with_css(css);
        let (cleared, _) = clear_conversation_state(&payload).expect("clear");
        let run = extract_ld(&cleared, 1).unwrap();
        let css2 = extract_ld(&run, 1).unwrap();
        assert!(css2.is_empty());
        crate::cursor::protocol::proto::agent::v1::AgentClientMessage::decode(cleared.as_slice())
            .expect("decode cleared");
    }
}


#[cfg(test)]
mod regression_run_inference {
    //! Cursor 3.27 (2026-10-29) sends aiserver.v1.RunInferenceClientMessage over
    //! BidiAppend. 1.0.2 rejected these as broken ACMs; 1.0.3 "recovered" them
    //! into an AgentRunRequest with no action (user text "继续下" lives in
    //! RunInferenceRunRequest.routing_conversation, field 4) -> empty Anthropic
    //! messages -> "assistant message prefill" 400 (req_011CgWNe9cM1XP4JJutMHDV8).
    use super::*;
    use crate::cursor::protocol::proto::agent::v1::AgentClientMessage;
    use prost::Message;

    const RAW_BB9C24B5: &str = "0ac7040a2435386361363331622d336563302d343630332d613930302d3230383336376532376664351a450a10643431353338633365633564626464631a0f0a07636f6e7465787412043530306b1a110a09726561736f6e696e671204686967681a0d0a0466617374120566616c73652a056167656e743a090a0764656661756c743a430a0867726f6b2d342e371a0f0a07636f6e7465787412043235366b1a180a10726561736f6e696e675f6566666f72741204686967681a0c0a04666173741204747275653a280a0867726f6b2d342e361a0d0a066566666f727412036c6f771a0d0a0466617374120566616c73653a450a10643431353338633365633564626464631a0f0a07636f6e7465787412043530306b1a110a09726561736f6e696e671204686967681a0d0a0466617374120566616c7365420b0a076578706c6f7265200150015ac3010a406239353139313165343437363532353038626136346234623230396365623537363237313065313432336465646466373038326136626539336263613763626310031a06737461626c6522286237396630623139336539643532666237336333376134663665363934636566373462343863636128023209647376332d313031383209647376332d31323035320b637572736f722d3032323632066c6174657374320a677074352d636f646578320b636f6465782d636c6f756432056861696b75622432636530613434362d383161332d343565362d396439392d3362656633386566643337318201056167656e74880101900101b00101c80101d80101";
    const RAW_008F9EB0: &str = "0acd070a2435386361363331622d336563302d343630332d613930302d3230383336376532376664351a450a10643431353338633365633564626464631a0f0a07636f6e7465787412043530306b1a110a09726561736f6e696e671204686967681a0d0a0466617374120566616c7365220d08011209e7bba7e7bbade4b88b2a056167656e743a090a0764656661756c743a430a0867726f6b2d342e371a0f0a07636f6e7465787412043235366b1a180a10726561736f6e696e675f6566666f72741204686967681a0c0a04666173741204747275653a280a0867726f6b2d342e361a0d0a066566666f727412036c6f771a0d0a0466617374120566616c73653a3f0a0f636c617564652d6f7075732d352d351a0d0a07636f6e746578741202316d1a0e0a066566666f72741204686967681a0d0a0466617374120566616c73653a2c0a0e67656d696e692d342d6172676f6e1a1a0a10726561736f6e696e675f6566666f727412066d656469756d3a3c0a0f6d7573652d776f6e6465722d312e301a0f0a07636f6e7465787412043330306b1a180a10726561736f6e696e675f6566666f72741204686967683a410a0b6770742d352e362d736f6c1a0f0a07636f6e7465787412043237326b1a120a09726561736f6e696e67120578686967681a0d0a0466617374120566616c73653a450a10636c617564652d6661626c652d352d311a100a087468696e6b696e671204747275651a0f0a07636f6e7465787412043330306b1a0e0a066566666f72741204686967683a3e0a11636c617564652d736f6e6e65742d352d351a0f0a07636f6e7465787412043330306b1a180a10726561736f6e696e675f6566666f72741204686967683a450a10643431353338633365633564626464631a0f0a07636f6e7465787412043530306b1a110a09726561736f6e696e671204686967681a0d0a0466617374120566616c7365420b0a076578706c6f7265200150015ac3010a406239353139313165343437363532353038626136346234623230396365623537363237313065313432336465646466373038326136626539336263613763626310031a06737461626c6522286237396630623139336539643532666237336333376134663665363934636566373462343863636128023209647376332d313031383209647376332d31323035320b637572736f722d3032323632066c6174657374320a677074352d636f646578320b636f6465782d636c6f756432056861696b75622432636530613434362d383161332d343565362d396439392d3362656633386566643337318201056167656e74880101900101a80101c80101d80101";

    fn always_ok(_: &[u8]) -> bool { true }

    #[test]
    fn wire_sanitize_run_inference_payloads_are_detected_and_not_repaired() {
        for hex_payload in [RAW_BB9C24B5, RAW_008F9EB0] {
            let raw = hex::decode(hex_payload).unwrap();
            assert!(AgentClientMessage::decode(raw.as_slice()).is_err());
            assert!(is_run_inference_client_message(&raw));
            assert!(sanitize_agent_client_message(&raw).is_none());
            assert!(clear_conversation_state(&raw).is_none());
            assert!(prune_run_request_fields(&raw, always_ok).is_none());
        }
    }

    #[test]
    fn wire_sanitize_real_acm_is_not_run_inference() {
        let css = ld_field(1, b"{}");
        let model = ld_field(1, b"m");
        let mut arr = ld_field(1, &css);
        arr.extend(ld_field(3, &model));
        let acm = ld_field(1, &arr);
        assert!(!is_run_inference_client_message(&acm));
        // Old 1.0.2 shape (UUID css but no model_details) also stays ACM.
        let acm_uuid = ld_field(1, &ld_field(1, b"58ca631b-3ec0-4603-a900-208367e27fd5"));
        assert!(!is_run_inference_client_message(&acm_uuid));
    }

    #[test]
    fn wire_sanitize_prune_refuses_to_drop_action() {
        let model = ld_field(1, b"m");
        let mut arr = ld_field(1, b"");
        arr.extend(ld_field(2, &[0x08, 0x01])); // action with garbage (varint in oneof slot)
        arr.extend(ld_field(3, &model));
        let acm = ld_field(1, &arr);
        let decodes = |b: &[u8]| AgentClientMessage::decode(b).is_ok();
        assert!(!decodes(&ld_field(1, &ld_field(2, &[0x08, 0x01]))));
        assert!(prune_run_request_fields(&acm, decodes).is_none());
    }
}
