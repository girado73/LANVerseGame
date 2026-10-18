//! Who a hello belongs to.

/// The authenticated identity behind a realtime connection.
///
/// Guests are allowed (the protocol makes `token` optional), but they have no
/// profile and cannot appear on a highscore.
#[derive(Debug, Clone, PartialEq)]
pub struct GameIdentity {
    pub player_name: String,
    pub profile_name: Option<String>,
    pub user_id: Option<i64>,
    pub player_profile_id: Option<i64>,
    pub is_guest: bool,
}

impl GameIdentity {
    pub fn guest(player_name: impl Into<String>) -> Self {
        Self {
            player_name: player_name.into(),
            profile_name: None,
            user_id: None,
            player_profile_id: None,
            is_guest: true,
        }
    }

    pub fn authenticated(
        requested_name: impl Into<String>,
        user_id: i64,
        player_profile_id: i64,
        profile_name: Option<String>,
    ) -> Self {
        Self {
            player_name: requested_name.into(),
            profile_name,
            user_id: Some(user_id),
            player_profile_id: Some(player_profile_id),
            is_guest: false,
        }
    }

    /// The name rendered in snapshots: the control-plane profile wins, so a
    /// hello name cannot impersonate another account.
    pub fn display_name(&self) -> String {
        match &self.profile_name {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => self.player_name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthError {
    pub message: String,
}

impl AuthError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Resolves a token from a `hello` into a game identity.
pub trait IdentityResolver: Send + Sync {
    fn resolve(&self, token: Option<&str>, requested_name: &str)
    -> Result<GameIdentity, AuthError>;
}

/// Guests only: any token is rejected. The binary installs the database-backed
/// resolver.
pub struct GuestOnlyResolver;

impl GuestOnlyResolver {
    fn sanitize(name: &str) -> String {
        let cleaned: String = name
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        let trimmed = cleaned.trim();

        if trimmed.is_empty() {
            "Player".to_owned()
        } else {
            trimmed.chars().take(32).collect()
        }
    }
}

impl IdentityResolver for GuestOnlyResolver {
    fn resolve(
        &self,
        token: Option<&str>,
        requested_name: &str,
    ) -> Result<GameIdentity, AuthError> {
        match token {
            None | Some("") => Ok(GameIdentity::guest(Self::sanitize(requested_name))),
            Some(_) => Err(AuthError::new("Invalid token.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_name_wins() {
        let identity = GameIdentity::authenticated("Admin", 1, 2, Some("Alice".into()));
        assert_eq!(identity.display_name(), "Alice");

        let identity = GameIdentity::authenticated("Admin", 1, 2, Some("  ".into()));
        assert_eq!(identity.display_name(), "Admin");
    }

    #[test]
    fn guest_names_are_sanitized() {
        let identity = GuestOnlyResolver.resolve(None, "  Ali\u{1}ce  ").unwrap();
        assert_eq!(identity.player_name, "Alice");
        assert!(identity.is_guest);

        let identity = GuestOnlyResolver.resolve(None, "   ").unwrap();
        assert_eq!(identity.player_name, "Player");
    }

    #[test]
    fn tokens_are_refused_without_a_database() {
        let error = GuestOnlyResolver
            .resolve(Some("1|abc"), "Alice")
            .unwrap_err();
        assert_eq!(error.message, "Invalid token.");
    }
}
