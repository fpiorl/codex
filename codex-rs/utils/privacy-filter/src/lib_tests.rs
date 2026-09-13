use super::*;
use pretty_assertions::assert_eq;

fn filter() -> PrivacyFilter {
    PrivacyFilter::new([
        PrivacyRule {
            real: "acme.com".into(),
            placeholder: "company-a.example".into(),
        },
        PrivacyRule {
            real: "Acme Corp".into(),
            placeholder: "Company A".into(),
        },
        PrivacyRule {
            real: "Acme".into(),
            placeholder: "CompanyA".into(),
        },
    ])
}

#[test]
fn redacts_domains_including_subdomains() {
    let f = filter();
    assert_eq!(
        f.redact("curl https://api.acme.com/v1 and ACME.COM"),
        "curl https://api.company-a.example/v1 and COMPANY-A.EXAMPLE"
    );
}

#[test]
fn longest_rule_wins() {
    let f = filter();
    assert_eq!(f.redact("Acme Corp owns acme"), "Company A owns companya");
}

#[test]
fn restore_round_trips() {
    let f = filter();
    let original = "Deploy Acme Corp to api.acme.com for Acme";
    let redacted = f.redact(original);
    assert_eq!(
        redacted,
        "Deploy Company A to api.company-a.example for CompanyA"
    );
    assert_eq!(f.restore(&redacted), original);
}

#[test]
fn restore_preserves_case_shape() {
    let f = filter();
    assert_eq!(
        f.restore("COMPANY A and company a"),
        "ACME CORP and acme corp"
    );
}

#[test]
fn untouched_text_is_unchanged() {
    let f = filter();
    assert_eq!(f.redact("nothing here"), "nothing here");
    let mut s = String::from("nothing");
    assert!(!f.redact_in_place(&mut s));
}

#[test]
fn redacts_response_item_strings() {
    let f = filter();
    let mut item: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "function_call",
        "name": "shell",
        "arguments": "{\"command\":[\"curl\",\"acme.com\"]}",
        "call_id": "call_1"
    }))
    .unwrap();
    f.redact_item(&mut item);
    let ResponseItem::FunctionCall { arguments, .. } = &item else {
        panic!("expected function call");
    };
    assert_eq!(arguments, "{\"command\":[\"curl\",\"company-a.example\"]}");
    f.restore_item(&mut item);
    let ResponseItem::FunctionCall { arguments, .. } = &item else {
        panic!("expected function call");
    };
    assert_eq!(arguments, "{\"command\":[\"curl\",\"acme.com\"]}");
}

#[test]
fn stream_restorer_handles_split_placeholder() {
    let f = filter();
    let mut r = f.stream_restorer();
    let mut out = String::new();
    out.push_str(&r.push("visit comp"));
    assert_eq!(out, "visit ");
    out.push_str(&r.push("any-a.exa"));
    out.push_str(&r.push("mple now, Comp"));
    assert_eq!(out, "visit acme.com now, ");
    out.push_str(&r.push("uter"));
    out.push_str(&r.flush());
    assert_eq!(out, "visit acme.com now, Computer");
}

#[test]
fn stream_restorer_flush_releases_pending_prefix() {
    let f = filter();
    let mut r = f.stream_restorer();
    let first = r.push("hello Company");
    assert_eq!(first, "hello ");
    assert!(r.has_pending());
    assert_eq!(r.flush(), "Company");
}

#[test]
fn stream_restorer_handles_placeholder_at_end_of_delta() {
    let f = filter();
    let mut r = f.stream_restorer();
    let mut out = r.push("see Company A");
    out.push_str(&r.push(" today"));
    out.push_str(&r.flush());
    assert_eq!(out, "see Acme Corp today");
}
