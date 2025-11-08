use tebex::webhooks::{TebexWebhookPayload, WebhookType};

const PAYLOADS: &[&str] = &[
	include_str!("payloads/payment-completed-1.json"),
	include_str!("payloads/payment-completed-2.json"),
	include_str!("payloads/validation.json")
];

#[test]
fn test_parsing() {
	for test in PAYLOADS {
		let Ok(parsed) = serde_json::from_str::<TebexWebhookPayload>(test) else {
			panic!("Unable to parse payload:\n\n{test}")
		};

		dbg!(&parsed);

		assert!(
			!matches!(parsed.webhook_type, WebhookType::Unknown { .. }),
			"Parsed test subject should not deserialize to WebhookPayload::Unknown"
		)
	}
}
