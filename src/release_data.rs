use serde::Deserialize;

pub const RELEASE_API: &str = "https://api.github.com/repos/asd123ch/rustquit/releases/latest";
pub const RELEASES_URL: &str = "https://github.com/asd123ch/rustquit/releases";
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    Available { version: String, url: String },
    Unavailable,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidRelease;

pub fn parse_release(
    status: u16,
    final_url: &str,
    body: &[u8],
    current: &str,
) -> Result<UpdateStatus, InvalidRelease> {
    if final_url != RELEASE_API || body.len() > MAX_RESPONSE_BYTES {
        return Err(InvalidRelease);
    }
    if status == 404 {
        return Ok(UpdateStatus::UpToDate);
    }
    if status != 200 {
        return Err(InvalidRelease);
    }
    let release: Release = serde_json::from_slice(body).map_err(|_| InvalidRelease)?;
    let version = semver::Version::parse(release.tag_name.strip_prefix('v').ok_or(InvalidRelease)?)
        .map_err(|_| InvalidRelease)?;
    let expected_url = format!("{RELEASES_URL}/tag/v{version}");
    if release.draft
        || release.prerelease
        || !version.pre.is_empty()
        || !version.build.is_empty()
        || release.html_url != expected_url
    {
        return Err(InvalidRelease);
    }
    if version > semver::Version::parse(current).map_err(|_| InvalidRelease)? {
        Ok(UpdateStatus::Available {
            version: version.to_string(),
            url: expected_url,
        })
    } else {
        Ok(UpdateStatus::UpToDate)
    }
}
