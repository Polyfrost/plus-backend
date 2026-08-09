/// Matched case-insensitively, so entries must be lowercase.
const BOT_UA_MARKERS: &[&str] = &[
	"bot",
	"crawler",
	"spider",
	"slurp",
	"preview",
	"unfurl",
	"embed",
	"facebookexternalhit",
	"discord",
	"twitter",
	"telegram",
	"whatsapp",
	"slack",
	"skype",
	"linkedin",
	"pinterest",
	"redditbot",
	"mastodon",
	"curl",
	"wget",
	"python-requests",
	"go-http-client",
	"headless",
];

/// Whether `user_agent` looks like a crawler, unfurler or scripted client.
/// A missing or blank user-agent counts as a bot.
pub(crate) fn is_bot(user_agent: &str) -> bool {
	if user_agent.trim().is_empty() {
		return true;
	}

	let ua = user_agent.to_ascii_lowercase();
	BOT_UA_MARKERS.iter().any(|marker| ua.contains(marker))
}

#[cfg(test)]
mod tests {
	use super::is_bot;

	#[test]
	fn browser_user_agents_are_not_bots() {
		let chrome = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
			AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
		let firefox =
			"Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
		assert!(!is_bot(chrome));
		assert!(!is_bot(firefox));
	}

	#[test]
	fn unfurl_and_scripted_agents_are_bots() {
		assert!(is_bot("")); // empty ua
		assert!(is_bot("   "));
		assert!(is_bot("Discordbot/2.0 (+https://discordapp.com)"));
		assert!(is_bot("Twitterbot/1.0"));
		assert!(is_bot("facebookexternalhit/1.1"));
		assert!(is_bot("TelegramBot (like TwitterBot)"));
		assert!(is_bot("Slackbot-LinkExpanding 1.0"));
		assert!(is_bot("curl/8.4.0"));
		assert!(is_bot("python-requests/2.31.0"));
	}
}
