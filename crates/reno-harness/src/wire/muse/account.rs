//! `account/read` as a sign-in state.
//!
//! The method is experimental in 1.4, so the handshake that asks it opts into `experimentalApi`.
//! It only reads: which credential lane Muse would use (`state`), a display `label` that never
//! carries key material, and whether the endpoint needs a credential at all. Muse owns its tokens
//! end to end (it mints, refreshes and locks its own store); Reno asks and never reads, copies,
//! refreshes or writes one. The sign-in itself (`muse login`, a device-code flow in the owner's
//! browser) is a person's step and `account/loginStart` is never sent.
//!
//! What the host consults to answer is its own business: on macOS the default store is the
//! Keychain, which Muse reads as itself. Reno does not touch it.

use reno_proto::SignedIn;
use serde_json::Value;

use super::frames::string;

/// What a person does about a missing sign-in.
pub(crate) const SIGN_IN_HINT: &str = "run `muse login` in a terminal and approve the code in \
    your browser";

/// The sign-in an `account/read` result means.
///
/// `loggedOut` is a missing sign-in unless the endpoint needs no credential
/// (`credentialRequired: false`, a keyless deployment). `envKey`, `apiKey` and `accountLogin` are
/// the three lanes Muse knows; the vocabulary is open, so another value is not a guess either
/// way.
pub(crate) fn signed_in(result: &Value) -> SignedIn {
    if result["credentialRequired"] == false {
        return SignedIn::Yes { account: None };
    }
    match string(result, "state") {
        "loggedOut" => SignedIn::No {
            hint: SIGN_IN_HINT.to_owned(),
        },
        "envKey" | "apiKey" | "accountLogin" => SignedIn::Yes {
            account: result["label"]
                .as_str()
                .filter(|l| !l.is_empty())
                .map(str::to_owned),
        },
        _ => SignedIn::Unknown,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_three_credential_lanes_are_signed_in_and_logged_out_is_not() {
        let login = signed_in(&json!({"state": "accountLogin", "label": "a@b.c",
            "credentialRequired": true}));
        assert_eq!(
            login,
            SignedIn::Yes {
                account: Some("a@b.c".into())
            }
        );
        for lane in ["envKey", "apiKey"] {
            assert_eq!(
                signed_in(&json!({"state": lane, "credentialRequired": true})),
                SignedIn::Yes { account: None }
            );
        }
        assert!(matches!(
            signed_in(&json!({"state": "loggedOut", "credentialRequired": true})),
            SignedIn::No { hint } if hint.contains("muse login")
        ));
        // No state at all is as good as a missing one.
        assert!(matches!(signed_in(&json!({})), SignedIn::Unknown));
    }

    #[test]
    fn a_keyless_deployment_needs_no_sign_in_and_a_new_lane_is_unknown() {
        assert_eq!(
            signed_in(&json!({"state": "loggedOut", "credentialRequired": false})),
            SignedIn::Yes { account: None }
        );
        assert_eq!(
            signed_in(&json!({"state": "hardwareToken", "credentialRequired": true})),
            SignedIn::Unknown
        );
    }
}
