use std::marker::PhantomData;
use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::llm::{LlmConfig, ServiceTier};
use crate::live_config::LiveConfig;
use crate::smt::SolverConfig;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub llm: LlmConfig,
    pub solver: SolverConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum ServiceTierArg {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
}

impl From<ServiceTierArg> for ServiceTier {
    fn from(arg: ServiceTierArg) -> Self {
        match arg {
            ServiceTierArg::Auto => ServiceTier::Auto,
            ServiceTierArg::Default => ServiceTier::Default,
            ServiceTierArg::Flex => ServiceTier::Flex,
            ServiceTierArg::Scale => ServiceTier::Scale,
            ServiceTierArg::Priority => ServiceTier::Priority,
        }
    }
}

#[derive(Parser)]
#[command(name = "set", about = "Update runtime configuration")]
pub struct UpdateArgs {
    #[arg(long, help = "Override the default API key for all roles")]
    pub api_key: Option<String>,

    #[arg(long, help = "API base URL")]
    pub api_base: Option<String>,

    #[arg(long, help = "Set ALL model names (individual --*-model overrides applied after)")]
    pub api_model: Option<String>,

    #[arg(long, help = "Model for the splitter role")]
    pub splitter_model: Option<String>,

    #[arg(long, help = "Model for the formalizer role")]
    pub formalizer_model: Option<String>,

    #[arg(long, help = "Model for the fixer role")]
    pub fixer_model: Option<String>,

    #[arg(long, help = "Model for the judge role")]
    pub judge_model: Option<String>,

    #[arg(long, help = "Model for the splitting judge role")]
    pub splitting_judge_model: Option<String>,

    #[arg(long, help = "Model for the analyzer role")]
    pub analyzer_model: Option<String>,

    #[arg(long, help = "Dedicated API key for the judge role")]
    pub judge_api_key: Option<String>,

    #[arg(long, help = "Dedicated API key for the formalizer role")]
    pub formalizer_api_key: Option<String>,

    #[arg(long, help = "Dedicated API key for the fixer role")]
    pub fixer_api_key: Option<String>,

    #[arg(long, help = "Dedicated API key for the splitter role")]
    pub splitter_api_key: Option<String>,

    #[arg(long, help = "Dedicated API key for the splitting judge role")]
    pub splitting_judge_api_key: Option<String>,

    #[arg(long, help = "Dedicated API key for the analyzer role")]
    pub analyzer_api_key: Option<String>,

    #[arg(long, help = "Per-token timeout in milliseconds (default: 3000)")]
    pub stream_timeout_ms: Option<u64>,

    #[arg(long, help = "Maximum stream retry attempts (default: 5)")]
    pub max_stream_retries: Option<u32>,

    #[arg(long, value_enum, help = "Service tier: auto, default, flex, scale, priority")]
    pub service_tier: Option<ServiceTierArg>,

    #[arg(long, help = "Path to the SMT solver binary")]
    pub solver_path: Option<String>,

    #[arg(long, num_args = 0.., help = "Arguments passed to the solver")]
    pub solver_args: Option<Vec<String>>,

    #[arg(long, help = "Solver timeout in seconds")]
    pub solver_timeout_secs: Option<u64>,
}

pub trait ApplyTo<C>: Parser + Send + Sync + 'static {
    fn apply_to(&self, config: &mut C);
}

impl ApplyTo<LlmConfig> for UpdateArgs {
    fn apply_to(&self, config: &mut LlmConfig) {
        if let Some(key) = &self.api_key {
            config.api_key = key.clone();
        }
        if let Some(base) = &self.api_base {
            config.api_base = base.clone();
        }
        if let Some(model) = &self.api_model {
            config.formalizer_model = model.clone();
            config.fixer_model = model.clone();
            config.judge_model = model.clone();
            config.splitting_judge_model = model.clone();
            config.splitter_model = model.clone();
            config.analyzer_model = model.clone();
        }
        if let Some(m) = &self.splitter_model {
            config.splitter_model = m.clone();
        }
        if let Some(m) = &self.formalizer_model {
            config.formalizer_model = m.clone();
        }
        if let Some(m) = &self.fixer_model {
            config.fixer_model = m.clone();
        }
        if let Some(m) = &self.judge_model {
            config.judge_model = m.clone();
        }
        if let Some(m) = &self.splitting_judge_model {
            config.splitting_judge_model = m.clone();
        }
        if let Some(m) = &self.analyzer_model {
            config.analyzer_model = m.clone();
        }
        if let Some(key) = &self.judge_api_key {
            config.judge_api_key = Some(key.clone());
        }
        if let Some(key) = &self.formalizer_api_key {
            config.formalizer_api_key = Some(key.clone());
        }
        if let Some(key) = &self.fixer_api_key {
            config.fixer_api_key = Some(key.clone());
        }
        if let Some(key) = &self.splitter_api_key {
            config.splitter_api_key = Some(key.clone());
        }
        if let Some(key) = &self.splitting_judge_api_key {
            config.splitting_judge_api_key = Some(key.clone());
        }
        if let Some(key) = &self.analyzer_api_key {
            config.analyzer_api_key = Some(key.clone());
        }
        if let Some(ms) = self.stream_timeout_ms {
            config.stream_timeout_ms = ms;
        }
        if let Some(retries) = self.max_stream_retries {
            config.max_stream_retries = retries;
        }
        if let Some(tier) = self.service_tier.clone() {
            config.service_tier = tier.into();
        }
    }
}

impl ApplyTo<SolverConfig> for UpdateArgs {
    fn apply_to(&self, config: &mut SolverConfig) {
        if let Some(path) = &self.solver_path {
            config.solver_path = path.clone();
        }
        if let Some(args) = &self.solver_args {
            config.solver_args = args.clone();
        }
        if let Some(secs) = self.solver_timeout_secs {
            config.timeout_secs = secs;
        }
    }
}

impl ApplyTo<AppConfig> for UpdateArgs {
    fn apply_to(&self, config: &mut AppConfig) {
        ApplyTo::<LlmConfig>::apply_to(self, &mut config.llm);
        ApplyTo::<SolverConfig>::apply_to(self, &mut config.solver);
    }
}

pub struct SetPlugin<A, C>
where
    A: ApplyTo<C>,
    C: Clone + Send + Sync + 'static,
{
    name: &'static str,
    config: Arc<LiveConfig<C>>,
    _phantom: PhantomData<A>,
}

impl<A, C> SetPlugin<A, C>
where
    A: ApplyTo<C>,
    C: Clone + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(name: &'static str, config: Arc<LiveConfig<C>>) -> Self {
        Self { name, config, _phantom: PhantomData }
    }

    pub fn handle(&self, args: &str) -> String {
        let mut tokens = vec![self.name];
        tokens.extend(args.split_whitespace());
        match A::try_parse_from(tokens) {
            Ok(update) => {
                let version = self.config.update(|c| update.apply_to(c));
                format!("[config updated to v{version}]")
            }
            Err(e) => e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmConfig;

    fn default_llm_config() -> LlmConfig {
        LlmConfig {
            api_key: "original-key".to_string(),
            judge_api_key: None,
            formalizer_api_key: None,
            fixer_api_key: None,
            splitter_api_key: None,
            splitting_judge_api_key: None,
            analyzer_api_key: None,
            api_base: "https://original.api/v1".to_string(),
            formalizer_model: "original-formalizer".to_string(),
            fixer_model: "original-fixer".to_string(),
            judge_model: "original-judge".to_string(),
            splitting_judge_model: "original-splitting-judge".to_string(),
            splitter_model: "original-splitter".to_string(),
            analyzer_model: "original-analyzer".to_string(),
            stream_timeout_ms: 3000,
            max_stream_retries: 5,
            service_tier: ServiceTier::Priority,
        }
    }

    fn default_solver_config() -> SolverConfig {
        SolverConfig {
            solver_path: "z3".to_string(),
            solver_args: vec!["-in".to_string()],
            timeout_secs: 60,
        }
    }

    #[test]
    fn test_parse_api_key() {
        let args = UpdateArgs::try_parse_from(["set", "--api-key", "new-key"]).unwrap();
        assert_eq!(args.api_key.as_deref(), Some("new-key"));

        let mut cfg = default_llm_config();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.api_key, "new-key");
    }

    #[test]
    fn test_parse_api_model_sets_all() {
        let args = UpdateArgs::try_parse_from(["set", "--api-model", "gpt-4"]).unwrap();

        let mut cfg = default_llm_config();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.formalizer_model, "gpt-4");
        assert_eq!(cfg.fixer_model, "gpt-4");
        assert_eq!(cfg.judge_model, "gpt-4");
        assert_eq!(cfg.splitting_judge_model, "gpt-4");
        assert_eq!(cfg.splitter_model, "gpt-4");
        assert_eq!(cfg.analyzer_model, "gpt-4");
    }

    #[test]
    fn test_api_model_then_individual_override() {
        let args =
            UpdateArgs::try_parse_from(["set", "--api-model", "gpt-4", "--judge-model", "llama"])
                .unwrap();

        let mut cfg = default_llm_config();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.formalizer_model, "gpt-4");
        assert_eq!(cfg.judge_model, "llama");
        assert_eq!(cfg.fixer_model, "gpt-4");
    }

    #[test]
    fn test_parse_service_tier() {
        let args = UpdateArgs::try_parse_from(["set", "--service-tier", "flex"]).unwrap();

        let mut cfg = default_llm_config();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.service_tier, ServiceTier::Flex);
    }

    #[test]
    fn test_parse_stream_timeout() {
        let args =
            UpdateArgs::try_parse_from(["set", "--stream-timeout-ms", "8000"]).unwrap();

        let mut cfg = default_llm_config();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.stream_timeout_ms, 8000);
    }

    #[test]
    fn test_parse_solver_config() {
        let args = UpdateArgs::try_parse_from([
            "set",
            "--solver-path",
            "/usr/bin/z3",
            "--solver-timeout-secs",
            "120",
        ])
        .unwrap();

        let mut cfg = default_solver_config();
        ApplyTo::<SolverConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.solver_path, "/usr/bin/z3");
        assert_eq!(cfg.timeout_secs, 120);
    }

    #[test]
    fn test_parse_per_role_keys() {
        let args = UpdateArgs::try_parse_from([
            "set",
            "--judge-api-key",
            "judge-key",
            "--fixer-api-key",
            "fixer-key",
        ])
        .unwrap();

        let mut cfg = default_llm_config();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.judge_api_key.as_deref(), Some("judge-key"));
        assert_eq!(cfg.fixer_api_key.as_deref(), Some("fixer-key"));
    }

    #[test]
    fn test_unspecified_fields_unchanged() {
        let args = UpdateArgs::try_parse_from(["set", "--api-key", "new"]).unwrap();

        let mut cfg = default_llm_config();
        let original_model = cfg.formalizer_model.clone();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.formalizer_model, original_model);
        assert_eq!(cfg.stream_timeout_ms, 3000);
    }

    #[test]
    fn test_invalid_service_tier_rejected() {
        let result = UpdateArgs::try_parse_from(["set", "--service-tier", "invalid"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_no_args_is_ok() {
        let args = UpdateArgs::try_parse_from(["set"]).unwrap();
        let mut cfg = default_llm_config();
        ApplyTo::<LlmConfig>::apply_to(&args, &mut cfg);
        assert_eq!(cfg.api_key, "original-key");
    }

    #[test]
    fn test_service_tier_arg_conversion() {
        assert_eq!(ServiceTierArg::Auto, ServiceTierArg::Auto);
        let tier: ServiceTier = ServiceTierArg::Flex.into();
        assert_eq!(tier, ServiceTier::Flex);
    }
}
