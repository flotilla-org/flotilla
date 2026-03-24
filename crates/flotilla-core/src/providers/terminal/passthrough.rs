use async_trait::async_trait;
use flotilla_protocol::arg::Arg;

use super::{TerminalEnvVars, TerminalPool, TerminalSession};
use crate::path_context::ExecutionEnvironmentPath;

pub struct PassthroughTerminalPool;

#[async_trait]
impl TerminalPool for PassthroughTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        Ok(vec![])
    }

    async fn ensure_session(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<(), String> {
        Ok(())
    }

    fn attach_args(
        &self,
        _session_name: &str,
        command: &str,
        _cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        let mut args = Vec::new();
        if !env_vars.is_empty() {
            args.push(Arg::Literal("env".into()));
            for (k, v) in env_vars {
                args.push(Arg::Literal(format!("{k}={}", flotilla_protocol::arg::shell_quote(v))));
            }
        }
        args.push(Arg::Literal(command.into()));
        Ok(args)
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_context::ExecutionEnvironmentPath;

    #[tokio::test]
    async fn list_returns_empty() {
        let pool = PassthroughTerminalPool;
        let sessions = pool.list_sessions().await.unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn ensure_is_noop() {
        let pool = PassthroughTerminalPool;
        assert!(pool.ensure_session("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &vec![]).await.is_ok());
    }

    #[tokio::test]
    async fn attach_passes_through() {
        let pool = PassthroughTerminalPool;
        let result = pool.attach_command("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &vec![]).await.unwrap();
        assert_eq!(result, "bash");
    }

    #[tokio::test]
    async fn attach_injects_env_vars() {
        let pool = PassthroughTerminalPool;
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let result = pool.attach_command("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &env).await.unwrap();
        assert!(result.starts_with("env "));
        assert!(result.contains("FOO='bar'"));
        assert!(result.ends_with("bash"));
    }

    #[tokio::test]
    async fn kill_is_noop() {
        let pool = PassthroughTerminalPool;
        assert!(pool.kill_session("my-session").await.is_ok());
    }

    // ── attach_args tests ──────────────────────────────────────────

    #[test]
    fn attach_args_simple_command() {
        let pool = PassthroughTerminalPool;
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &vec![]).expect("attach_args");

        assert_eq!(args, vec![Arg::Literal("bash".into())]);
    }

    #[test]
    fn attach_args_flatten_simple_command() {
        let pool = PassthroughTerminalPool;
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &vec![]).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "bash");
    }

    #[test]
    fn attach_args_with_env_vars() {
        let pool = PassthroughTerminalPool;
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &env).expect("attach_args");

        assert_eq!(args, vec![Arg::Literal("env".into()), Arg::Literal("FOO='bar'".into()), Arg::Literal("bash".into()),]);
    }

    #[test]
    fn attach_args_flatten_with_env_vars() {
        let pool = PassthroughTerminalPool;
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &env).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "env FOO='bar' bash");
    }

    #[test]
    fn attach_args_flatten_matches_attach_command() {
        // Regression: flatten(attach_args()) should produce the same string as attach_command()
        let pool = PassthroughTerminalPool;
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/tmp"), &env).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        // The old attach_command produced: "env FOO='bar' bash"
        assert_eq!(flat, "env FOO='bar' bash");
    }

    #[test]
    fn attach_args_env_value_with_single_quote() {
        let pool = PassthroughTerminalPool;
        let env = vec![("KEY".to_string(), "it's".to_string())];
        let args = pool.attach_args("s", "cmd", &ExecutionEnvironmentPath::new("/tmp"), &env).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "env KEY='it'\\''s' cmd");
    }
}
