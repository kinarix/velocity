//! Pod metadata + log line parsing.
//!
//! Pod directories under `/var/log/pods` follow the kubelet convention:
//!
//!   `{namespace}_{pod_name}_{uid}`
//!
//! Each container has its own subdirectory; logs land in
//! `{container}/0.log` (rotated as `0.log.YYYYMMDD-HHMMSS`).
//!
//! Log lines are either:
//! - structured JSON (most Velocity components), in which case we
//!   forward as-is and let the processor's enrich/filter walk into it;
//! - CRI text format `2026-05-19T20:34:01.123456789Z stdout F message`,
//!   wrapped into `{ "message": "...", "stream": "stdout|stderr",
//!   "timestamp": "..." }`;
//! - or arbitrary text, wrapped into `{ "message": "..." }`.

use serde_json::{json, Value};

/// Metadata derived from a pod directory name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodMeta {
    pub namespace: String,
    pub pod: String,
    pub uid: String,
}

/// Parse a `{namespace}_{pod}_{uid}` directory name. Returns `None`
/// for anything that doesn't match — we silently skip non-conforming
/// paths so an unrelated file under the log root can't crash us.
pub fn parse_pod_dir(dir_name: &str) -> Option<PodMeta> {
    // The pod name itself may contain underscores (a Deployment's
    // ReplicaSet hash suffix uses `-`, but other resource templates
    // can produce names with `_`). Split off the trailing UID, then
    // namespace, then everything between is the pod name.
    //
    // UIDs are 36-char dashed v4 UUIDs in standard form. We don't
    // require strict UUID parsing — we look for the last underscore
    // before a 36-char tail that contains dashes.
    let bytes = dir_name.as_bytes();
    if bytes.len() < 38 {
        return None;
    }
    let uid_start = bytes.len() - 36;
    if uid_start == 0 || bytes[uid_start - 1] != b'_' {
        return None;
    }
    let uid = &dir_name[uid_start..];
    if !uid.contains('-') {
        return None;
    }
    let head = &dir_name[..uid_start - 1];
    let (namespace, pod) = head.split_once('_')?;
    if namespace.is_empty() || pod.is_empty() {
        return None;
    }
    Some(PodMeta {
        namespace: namespace.to_string(),
        pod: pod.to_string(),
        uid: uid.to_string(),
    })
}

/// Parse a single log line, returning the JSON record we'll forward.
/// Adds the supplied pod metadata under `kubernetes.*` so the
/// processor's enrich step can find the labels (in v1 we don't
/// resolve labels from the kube API — they arrive via a sidecar or
/// the operator-supplied ConfigMap that maps pod → labels).
pub fn parse_line(raw: &str, container: &str, meta: &PodMeta) -> Value {
    let body = parse_body(raw);
    let mut envelope = if let Value::Object(_) = &body {
        body
    } else {
        json!({ "message": body })
    };
    if let Value::Object(map) = &mut envelope {
        map.insert(
            "kubernetes".to_string(),
            json!({
                "namespace": meta.namespace,
                "pod": meta.pod,
                "uid": meta.uid,
                "container": container,
            }),
        );
    }
    envelope
}

/// Try in this order: structured JSON, CRI text, fallback to a wrapped
/// `message: raw` string.
fn parse_body(raw: &str) -> Value {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return v;
    }
    if let Some(cri) = parse_cri_text(raw) {
        return cri;
    }
    Value::String(raw.to_string())
}

/// CRI text format: `<timestamp> <stream> <P|F> <message>`. `P` means
/// a partial line (multi-line not joined here — v2). We accept both
/// shapes; the processor sees the partial flag if it wants to drop.
fn parse_cri_text(raw: &str) -> Option<Value> {
    let mut parts = raw.splitn(4, ' ');
    let ts = parts.next()?;
    let stream = parts.next()?;
    let flag = parts.next()?;
    let msg = parts.next().unwrap_or("");
    // Sanity-check the prefix shapes. CRI streams are stdout|stderr;
    // flags are F|P. If anything looks off, bail and let the caller
    // fall back to the `message: raw` wrapper.
    if !matches!(stream, "stdout" | "stderr") || !matches!(flag, "F" | "P") {
        return None;
    }
    // Timestamp must at least look ISO-ish; we don't fully parse here.
    if !ts.contains('T') || ts.len() < 20 {
        return None;
    }
    Some(json!({
        "timestamp": ts,
        "stream": stream,
        "partial": flag == "P",
        "message": msg,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const UUID: &str = "12345678-1234-1234-1234-123456789abc";

    #[test]
    fn parses_simple_pod_dir() {
        let m = parse_pod_dir(&format!("velocity_api-7b9f-xyz_{UUID}")).unwrap();
        assert_eq!(m.namespace, "velocity");
        assert_eq!(m.pod, "api-7b9f-xyz");
        assert_eq!(m.uid, UUID);
    }

    #[test]
    fn rejects_short_name() {
        assert!(parse_pod_dir("too_short").is_none());
    }

    #[test]
    fn rejects_uid_without_dashes() {
        let bogus_uid = "x".repeat(36);
        let dir = format!("ns_pod_{bogus_uid}");
        assert!(parse_pod_dir(&dir).is_none());
    }

    #[test]
    fn rejects_missing_namespace_separator() {
        let dir = format!("nopod_{UUID}");
        // No second `_` so split_once on head fails.
        assert!(parse_pod_dir(&dir).is_none());
    }

    fn meta() -> PodMeta {
        PodMeta { namespace: "ns".into(), pod: "p".into(), uid: UUID.into() }
    }

    #[test]
    fn json_line_passthrough_with_kubernetes_envelope() {
        let raw = r#"{"level":"INFO","msg":"hi"}"#;
        let v = parse_line(raw, "main", &meta());
        assert_eq!(v["level"], json!("INFO"));
        assert_eq!(v["kubernetes"]["namespace"], json!("ns"));
        assert_eq!(v["kubernetes"]["container"], json!("main"));
    }

    #[test]
    fn cri_text_parsed_into_envelope() {
        let raw = "2026-05-19T20:34:01.123456789Z stdout F hello world";
        let v = parse_line(raw, "main", &meta());
        assert_eq!(v["stream"], json!("stdout"));
        assert_eq!(v["message"], json!("hello world"));
        assert_eq!(v["partial"], json!(false));
        assert_eq!(v["kubernetes"]["pod"], json!("p"));
    }

    #[test]
    fn cri_partial_flag_preserved() {
        let raw = "2026-05-19T20:34:01.123456789Z stderr P partial-line";
        let v = parse_line(raw, "c", &meta());
        assert_eq!(v["partial"], json!(true));
        assert_eq!(v["stream"], json!("stderr"));
    }

    #[test]
    fn plain_text_falls_back_to_message_wrapper() {
        let raw = "not json, not cri";
        let v = parse_line(raw, "c", &meta());
        assert_eq!(v["message"], json!("not json, not cri"));
        assert_eq!(v["kubernetes"]["namespace"], json!("ns"));
    }

    #[test]
    fn malformed_cri_falls_through_to_wrapper() {
        // Wrong stream name — should not be parsed as CRI.
        let raw = "2026-05-19T20:34:01Z badstream F whatever";
        let v = parse_line(raw, "c", &meta());
        assert_eq!(v["message"], json!(raw));
    }
}
