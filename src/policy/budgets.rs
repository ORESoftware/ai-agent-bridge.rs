fn budget_profile(risk: TaskRisk) -> BudgetProfile {
    match risk {
        TaskRisk::Low => BudgetProfile {
            max_providers: 1,
            max_rounds: 1,
            max_wall_clock_ms: 600_000,
            max_input_tokens: 100_000,
            max_output_tokens: 25_000,
            max_retries: 1,
            max_concurrency: 1,
            max_cost_micro_usd: 2_000_000,
        },
        TaskRisk::Medium => BudgetProfile {
            max_providers: 3,
            max_rounds: 2,
            max_wall_clock_ms: 1_800_000,
            max_input_tokens: 300_000,
            max_output_tokens: 75_000,
            max_retries: 2,
            max_concurrency: 3,
            max_cost_micro_usd: 10_000_000,
        },
        TaskRisk::High => BudgetProfile {
            max_providers: 4,
            max_rounds: 3,
            max_wall_clock_ms: 2_700_000,
            max_input_tokens: 750_000,
            max_output_tokens: 200_000,
            max_retries: 3,
            max_concurrency: 4,
            max_cost_micro_usd: 50_000_000,
        },
        TaskRisk::Critical => BudgetProfile {
            max_providers: 4,
            max_rounds: 3,
            max_wall_clock_ms: 3_600_000,
            max_input_tokens: 1_000_000,
            max_output_tokens: 250_000,
            max_retries: 3,
            max_concurrency: 4,
            max_cost_micro_usd: 100_000_000,
        },
    }
}

fn clamp_budget(requested: &RequestedBudget, profile: BudgetProfile) -> BudgetLimits {
    BudgetLimits {
        max_providers: clamp_u8(
            requested.max_providers,
            profile.max_providers,
            ABS_MAX_PROVIDERS,
        ),
        max_rounds: clamp_u8(requested.max_rounds, profile.max_rounds, ABS_MAX_ROUNDS),
        max_wall_clock_ms: clamp_u64(
            requested.max_wall_clock_ms,
            profile.max_wall_clock_ms,
            ABS_MAX_WALL_CLOCK_MS,
        ),
        max_input_tokens: clamp_u64(
            requested.max_input_tokens,
            profile.max_input_tokens,
            ABS_MAX_INPUT_TOKENS,
        ),
        max_output_tokens: clamp_u64(
            requested.max_output_tokens,
            profile.max_output_tokens,
            ABS_MAX_OUTPUT_TOKENS,
        ),
        max_retries: clamp_u8(requested.max_retries, profile.max_retries, ABS_MAX_RETRIES),
        max_concurrency: clamp_u8(
            requested.max_concurrency,
            profile.max_concurrency,
            ABS_MAX_CONCURRENCY,
        ),
        max_cost_micro_usd: clamp_u64(
            requested.max_cost_micro_usd,
            profile.max_cost_micro_usd,
            ABS_MAX_COST_MICRO_USD,
        ),
    }
}

fn clamp_u8(requested: Option<u8>, profile: u8, absolute: u8) -> u8 {
    requested.unwrap_or(profile).max(1).min(profile).min(absolute)
}

fn clamp_u64(requested: Option<u64>, profile: u64, absolute: u64) -> u64 {
    requested.unwrap_or(profile).max(1).min(profile).min(absolute)
}

const fn default_true() -> bool {
    true
}

const fn default_quality_score_bps() -> u16 {
    10_000
}

const fn default_health_score_bps() -> u16 {
    10_000
}
