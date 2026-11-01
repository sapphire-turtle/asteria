//! The background service that keeps the local server running.

use corundum_framework::service::ServiceSpec;

/// The service for `app`: corundum runs the app's own executable with `serve`, under the
/// app's name (a systemd user unit, a LaunchAgent, or a logon task).
pub fn service_spec(app: &'static str, description: &str) -> ServiceSpec {
    ServiceSpec {
        app_name: app,
        description: description.to_owned(),
        args: vec!["serve".to_owned()],
        legacy_labels: vec![],
        post_install: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_serve_under_the_apps_name() {
        let spec = service_spec("asteria-photos", "Asteria Photos server");
        assert_eq!(spec.app_name, "asteria-photos");
        assert_eq!(spec.description, "Asteria Photos server");
        assert_eq!(spec.args, ["serve"]);
        assert!(spec.legacy_labels.is_empty());
        assert!(spec.post_install.is_none());
    }
}
