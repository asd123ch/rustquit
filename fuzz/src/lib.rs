// Compile the production parsers directly, without AppKit, file access or networking.
#[path = "../../src/config_data.rs"]
pub mod config_data;
#[path = "../../src/release_data.rs"]
pub mod release_data;

use std::collections::HashSet;

use config_data::{normalize, parse_config};
use release_data::{RELEASE_API, RELEASES_URL, UpdateStatus, parse_release};

const NOW: u64 = 1_700_000_000;
const CURRENT: &str = "0.5.0";

pub fn check_config(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(config) = parse_config(text, NOW) else {
        return;
    };
    assert!(config.quit_delay_secs.is_finite());
    assert!((0.05..=10.0).contains(&config.quit_delay_secs));
    assert!(config.keep_alive_delay_secs.is_finite());
    assert!((1.0..=60.0).contains(&config.keep_alive_delay_secs));
    for list in [&config.apps, &config.keep_alive_apps] {
        assert!(list.len() <= 4096);
        let mut seen = HashSet::new();
        for id in list {
            assert!(valid_id(id));
            assert!(seen.insert(id.to_ascii_lowercase()));
        }
    }
    let kept: HashSet<_> = config
        .keep_alive_apps
        .iter()
        .map(|id| id.to_ascii_lowercase())
        .collect();
    assert!(
        config
            .apps
            .iter()
            .all(|id| !kept.contains(&id.to_ascii_lowercase()))
    );
    assert!(config.keep_alive_ignored_until.len() <= 4096);
    for (id, until) in &config.keep_alive_ignored_until {
        assert!(valid_id(id));
        assert_eq!(*id, id.to_ascii_lowercase());
        assert!(*until > NOW);
    }
    let mut normalized_again = config.clone();
    normalize(&mut normalized_again, NOW);
    assert_eq!(config, normalized_again);
    let encoded = toml::to_string_pretty(&config).expect("normalized config must serialize");
    assert_eq!(parse_config(&encoded, NOW).unwrap(), config);
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && id.contains('.')
        && !id.starts_with('.')
        && !id.ends_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
}

fn check_available(result: Result<UpdateStatus, release_data::InvalidRelease>) {
    if let Ok(UpdateStatus::Available { version, url }) = result {
        let parsed = semver::Version::parse(&version).unwrap();
        assert!(parsed > semver::Version::new(0, 5, 0));
        assert!(parsed.pre.is_empty());
        assert!(parsed.build.is_empty());
        assert_eq!(url, format!("{RELEASES_URL}/tag/v{parsed}"));
    }
}

pub fn check_update_release(data: &[u8]) {
    check_available(parse_release(200, RELEASE_API, data, CURRENT));
    for status in [200, 404] {
        assert!(parse_release(status, "https://example.invalid/redirect", data, CURRENT).is_err());
    }
    for status in [0, 301, 403, 429, 500, u16::MAX] {
        assert!(parse_release(status, RELEASE_API, data, CURRENT).is_err());
    }
    if data.len() <= release_data::MAX_RESPONSE_BYTES {
        assert_eq!(
            parse_release(404, RELEASE_API, data, CURRENT),
            Ok(UpdateStatus::UpToDate)
        );
    }
    // Reach version and URL checks even when the input is not a complete JSON document.
    if let Ok(version) = std::str::from_utf8(data) {
        let mut release = serde_json::json!({
            "tag_name": format!("v{version}"),
            "html_url": format!("{RELEASES_URL}/tag/v{version}"),
            "draft": false,
            "prerelease": false,
        });
        let body = serde_json::to_vec(&release).unwrap();
        check_available(parse_release(200, RELEASE_API, &body, CURRENT));
        assert!(parse_release(200, RELEASE_API, &body, "invalid").is_err());
        release["html_url"] = serde_json::json!("https://example.invalid/download");
        assert!(
            parse_release(
                200,
                RELEASE_API,
                &serde_json::to_vec(&release).unwrap(),
                CURRENT
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_config_seeds() {
        for data in [
            include_bytes!("../seeds/config/defaults").as_slice(),
            include_bytes!("../seeds/config/lists"),
            include_bytes!("../seeds/config/nonfinite"),
            include_bytes!("../seeds/config/malformed"),
        ] {
            check_config(data);
        }
    }

    #[test]
    fn synthetic_release_seeds() {
        for data in [
            include_bytes!("../seeds/update_release/newer").as_slice(),
            include_bytes!("../seeds/update_release/untrusted"),
            include_bytes!("../seeds/update_release/version"),
            include_bytes!("../seeds/update_release/prerelease"),
        ] {
            check_update_release(data);
        }
    }

    #[test]
    fn config_boundaries_and_keep_precedence() {
        let oversized = " ".repeat(config_data::MAX_CONFIG_BYTES + 1);
        assert!(parse_config(&oversized, NOW).is_err());
        assert!(parse_config(&oversized[..config_data::MAX_CONFIG_BYTES], NOW).is_ok());
        let text = "apps = ['com.example.App', 'com.EXAMPLE.app']\nkeep_alive_apps = ['COM.example.App']\n[keep_alive_ignored_until]\n'com.example.App' = 1800000000\n'COM.example.app' = 1900000000\n'com.example.expired' = 1";
        let config = parse_config(text, NOW).unwrap();
        assert!(config.apps.is_empty());
        assert_eq!(config.keep_alive_ignored_until.len(), 1);
        assert_eq!(
            config.keep_alive_ignored_until["com.example.app"],
            1_900_000_000
        );
        check_config(text.as_bytes());
        let ids: Vec<_> = (0..4100).map(|i| format!("com.example.app{i}")).collect();
        let text = format!("apps = {}", serde_json::to_string(&ids).unwrap());
        assert_eq!(parse_config(&text, NOW).unwrap().apps.len(), 4096);
        check_config(text.as_bytes());
    }

    #[test]
    fn release_response_size_limit() {
        let oversized = vec![b' '; release_data::MAX_RESPONSE_BYTES + 1];
        for status in [200, 404] {
            assert!(parse_release(status, RELEASE_API, &oversized, CURRENT).is_err());
        }
        assert_eq!(
            parse_release(
                404,
                RELEASE_API,
                &oversized[..release_data::MAX_RESPONSE_BYTES],
                CURRENT
            ),
            Ok(UpdateStatus::UpToDate)
        );
    }

    #[test]
    fn valid_release_is_accepted() {
        let data = include_bytes!("../seeds/update_release/newer");
        assert!(matches!(
            parse_release(200, RELEASE_API, data, CURRENT),
            Ok(UpdateStatus::Available { .. })
        ));
    }
}
