//! Tier exemplar library for goal classification (`CONDUCTOR_LOOP_PLAN.md` §3.1.1).

use std::path::Path;

use rinne_core::dag::ComplexityTier;
use serde::Deserialize;

/// One reference prompt that anchors a complexity tier.
#[derive(Debug, Clone, Copy)]
pub struct TierExemplar {
    pub id: &'static str,
    pub tier: ComplexityTier,
    pub prompt: &'static str,
    pub signals: &'static [&'static str],
}

/// All shipped exemplars (12 per tier).
pub fn all() -> &'static [TierExemplar] {
    EXEMPLARS
}

/// User-defined exemplar loaded from `.rinne/routing-exemplars.toml`.
#[derive(Debug, Clone)]
pub struct LoadedExemplar {
    pub id: String,
    pub tier: ComplexityTier,
    pub prompt: String,
    pub signals: Vec<String>,
}

/// Load optional project exemplars; missing or invalid files are ignored.
pub fn load_user_exemplars(path: &Path) -> Vec<LoadedExemplar> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    #[derive(Debug, Deserialize)]
    struct File {
        #[serde(default)]
        exemplar: Vec<Entry>,
    }
    #[derive(Debug, Deserialize)]
    struct Entry {
        id: String,
        tier: String,
        prompt: String,
        #[serde(default)]
        signals: Vec<String>,
    }
    let Ok(file) = toml::from_str::<File>(&raw) else {
        return Vec::new();
    };
    file.exemplar
        .into_iter()
        .filter_map(|e| {
            let tier = ComplexityTier::parse(&e.tier)?;
            Some(LoadedExemplar {
                id: e.id,
                tier,
                prompt: e.prompt,
                signals: e.signals,
            })
        })
        .collect()
}

/// Count exemplars per tier (shipped + user).
pub fn tier_counts(user: &[LoadedExemplar]) -> [usize; 5] {
    let mut counts = [0usize; 5];
    for ex in all() {
        counts[tier_index(ex.tier)] += 1;
    }
    for ex in user {
        counts[tier_index(ex.tier)] += 1;
    }
    counts
}

fn tier_index(t: ComplexityTier) -> usize {
    match t {
        ComplexityTier::T0 => 0,
        ComplexityTier::T1 => 1,
        ComplexityTier::T2 => 2,
        ComplexityTier::T3 => 3,
        ComplexityTier::T4 => 4,
    }
}

const EXEMPLARS: &[TierExemplar] = &[
    // T0
    TierExemplar {
        id: "T0-01",
        tier: ComplexityTier::T0,
        prompt: "Summarize what the billing module does in bullet points",
        signals: &["summarize", "explain"],
    },
    TierExemplar {
        id: "T0-02",
        tier: ComplexityTier::T0,
        prompt: "Write a commit message for the staged diff",
        signals: &["commit", "message"],
    },
    TierExemplar {
        id: "T0-03",
        tier: ComplexityTier::T0,
        prompt: "Explain the difference between Basic and Pro plans from pricing docs",
        signals: &["explain", "docs"],
    },
    TierExemplar {
        id: "T0-04",
        tier: ComplexityTier::T0,
        prompt: "Fix the typo recieve to receive in onboarding copy",
        signals: &["typo", "copy"],
    },
    TierExemplar {
        id: "T0-05",
        tier: ComplexityTier::T0,
        prompt: "Convert this curl example into a fetch snippet for the docs",
        signals: &["docs", "convert"],
    },
    TierExemplar {
        id: "T0-06",
        tier: ComplexityTier::T0,
        prompt: "Generate release notes from changelog entries this week",
        signals: &["release", "notes"],
    },
    TierExemplar {
        id: "T0-07",
        tier: ComplexityTier::T0,
        prompt: "What does the Retry-After header do in our API docs",
        signals: &["explain", "api"],
    },
    TierExemplar {
        id: "T0-08",
        tier: ComplexityTier::T0,
        prompt: "Reformat README tables so they render on GitHub",
        signals: &["format", "markdown"],
    },
    TierExemplar {
        id: "T0-09",
        tier: ComplexityTier::T0,
        prompt: "List environment variables referenced in docker-compose",
        signals: &["list", "env"],
    },
    TierExemplar {
        id: "T0-10",
        tier: ComplexityTier::T0,
        prompt: "Draft a FAQ answer about data export",
        signals: &["draft", "faq"],
    },
    TierExemplar {
        id: "T0-11",
        tier: ComplexityTier::T0,
        prompt: "Rename handleClick to handleSubmit in Button.tsx only",
        signals: &["rename", "single-file"],
    },
    TierExemplar {
        id: "T0-12",
        tier: ComplexityTier::T0,
        prompt: "Add a one-line doc comment above parseToken",
        signals: &["comment", "doc"],
    },
    // T1
    TierExemplar {
        id: "T1-01",
        tier: ComplexityTier::T1,
        prompt: "Fix off-by-one pagination bug on page 2 returning duplicates",
        signals: &["fix", "bug", "pagination"],
    },
    TierExemplar {
        id: "T1-02",
        tier: ComplexityTier::T1,
        prompt: "Add STRIPE_WEBHOOK_SECRET to env example and document it",
        signals: &["env", "docs"],
    },
    TierExemplar {
        id: "T1-03",
        tier: ComplexityTier::T1,
        prompt: "Fix broken import in Dashboard after package rename",
        signals: &["fix", "import"],
    },
    TierExemplar {
        id: "T1-04",
        tier: ComplexityTier::T1,
        prompt: "Add a unit test for normalizeEmail",
        signals: &["test", "unit"],
    },
    TierExemplar {
        id: "T1-05",
        tier: ComplexityTier::T1,
        prompt: "Bump patch version in package.json for hotfix",
        signals: &["version", "bump"],
    },
    TierExemplar {
        id: "T1-06",
        tier: ComplexityTier::T1,
        prompt: "Fix mobile nav overlap under 768px in header css",
        signals: &["fix", "css"],
    },
    TierExemplar {
        id: "T1-07",
        tier: ComplexityTier::T1,
        prompt: "Add max-length validation to signup email field",
        signals: &["validation", "form"],
    },
    TierExemplar {
        id: "T1-08",
        tier: ComplexityTier::T1,
        prompt: "Resolve TypeScript error in api-client.ts",
        signals: &["fix", "typescript"],
    },
    TierExemplar {
        id: "T1-09",
        tier: ComplexityTier::T1,
        prompt: "Add logging when webhook delivery fails",
        signals: &["logging", "webhook"],
    },
    TierExemplar {
        id: "T1-10",
        tier: ComplexityTier::T1,
        prompt: "Fix Dockerfile COPY path so cargo build works in CI",
        signals: &["docker", "fix"],
    },
    TierExemplar {
        id: "T1-11",
        tier: ComplexityTier::T1,
        prompt: "Update stale Jest snapshot for ProfileCard test",
        signals: &["test", "snapshot"],
    },
    TierExemplar {
        id: "T1-12",
        tier: ComplexityTier::T1,
        prompt: "Change default trial length from 14 to 30 days in config",
        signals: &["config", "trial"],
    },
    // T2
    TierExemplar {
        id: "T2-01",
        tier: ComplexityTier::T2,
        prompt: "Add POST invites endpoint with validation migration and integration test",
        signals: &["api", "endpoint", "test"],
    },
    TierExemplar {
        id: "T2-02",
        tier: ComplexityTier::T2,
        prompt: "Implement dark mode toggle with design tokens and localStorage",
        signals: &["feature", "ui"],
    },
    TierExemplar {
        id: "T2-03",
        tier: ComplexityTier::T2,
        prompt: "Add Stripe checkout.session.completed webhook with idempotency",
        signals: &["stripe", "webhook"],
    },
    TierExemplar {
        id: "T2-04",
        tier: ComplexityTier::T2,
        prompt: "Build user profile settings page with avatar upload",
        signals: &["feature", "ui", "upload"],
    },
    TierExemplar {
        id: "T2-05",
        tier: ComplexityTier::T2,
        prompt: "Add per-IP rate limiting middleware on public API routes",
        signals: &["rate-limit", "middleware", "api"],
    },
    TierExemplar {
        id: "T2-06",
        tier: ComplexityTier::T2,
        prompt: "Implement CSV export for orders table in admin panel",
        signals: &["feature", "admin", "export"],
    },
    TierExemplar {
        id: "T2-07",
        tier: ComplexityTier::T2,
        prompt: "Add Google OAuth sign-in without breaking existing sessions",
        signals: &["oauth", "auth"],
    },
    TierExemplar {
        id: "T2-08",
        tier: ComplexityTier::T2,
        prompt: "Build admin user search with debounced query and cursor pagination",
        signals: &["admin", "search", "api"],
    },
    TierExemplar {
        id: "T2-09",
        tier: ComplexityTier::T2,
        prompt: "Send welcome email when a workspace is created",
        signals: &["email", "integration"],
    },
    TierExemplar {
        id: "T2-10",
        tier: ComplexityTier::T2,
        prompt: "Add health ready endpoint checking Postgres and Redis",
        signals: &["health", "api"],
    },
    TierExemplar {
        id: "T2-11",
        tier: ComplexityTier::T2,
        prompt: "Implement feature flag new-checkout wrapping checkout component",
        signals: &["feature-flag", "ui"],
    },
    TierExemplar {
        id: "T2-12",
        tier: ComplexityTier::T2,
        prompt: "Add MCP tool listing to settings page wired to mcp client",
        signals: &["mcp", "ui", "feature"],
    },
    // T3
    TierExemplar {
        id: "T3-01",
        tier: ComplexityTier::T3,
        prompt: "Refactor billing god-class into subscription invoice and usage submodules",
        signals: &["refactor", "billing"],
    },
    TierExemplar {
        id: "T3-02",
        tier: ComplexityTier::T3,
        prompt: "Migrate notifications from synchronous sends to Redis queue with retries",
        signals: &["migrate", "queue", "redis"],
    },
    TierExemplar {
        id: "T3-03",
        tier: ComplexityTier::T3,
        prompt: "Replace session cookies with JWT access and refresh across API and middleware",
        signals: &["auth", "jwt", "refactor"],
    },
    TierExemplar {
        id: "T3-04",
        tier: ComplexityTier::T3,
        prompt: "Fix N+1 queries on dashboard with DataLoader and benchmark",
        signals: &["performance", "graphql", "refactor"],
    },
    TierExemplar {
        id: "T3-05",
        tier: ComplexityTier::T3,
        prompt: "Split shared package into core and ui without breaking downstream imports",
        signals: &["refactor", "monorepo"],
    },
    TierExemplar {
        id: "T3-06",
        tier: ComplexityTier::T3,
        prompt: "Upgrade React 17 to 18 and fix concurrent rendering in kanban",
        signals: &["upgrade", "react", "refactor"],
    },
    TierExemplar {
        id: "T3-07",
        tier: ComplexityTier::T3,
        prompt: "Implement multi-tenant row-level isolation with tenant_id on all tables",
        signals: &["multi-tenant", "database", "refactor"],
    },
    TierExemplar {
        id: "T3-08",
        tier: ComplexityTier::T3,
        prompt: "Add offline-first sync with conflict resolution for mobile companion",
        signals: &["sync", "mobile", "architecture"],
    },
    TierExemplar {
        id: "T3-09",
        tier: ComplexityTier::T3,
        prompt: "Consolidate three Stripe integrations into one billing adapter",
        signals: &["refactor", "stripe", "integration"],
    },
    TierExemplar {
        id: "T3-10",
        tier: ComplexityTier::T3,
        prompt: "Optimize search index rebuild from 45 minutes to under 5 minutes",
        signals: &["performance", "optimize"],
    },
    TierExemplar {
        id: "T3-11",
        tier: ComplexityTier::T3,
        prompt: "Introduce event bus for order lifecycle without downtime deploy",
        signals: &["event-bus", "architecture"],
    },
    TierExemplar {
        id: "T3-12",
        tier: ComplexityTier::T3,
        prompt: "Refactor conductor to tier routing matrix instead of prompt-only routing",
        signals: &["refactor", "architecture", "conductor"],
    },
    // T4
    TierExemplar {
        id: "T4-01",
        tier: ComplexityTier::T4,
        prompt: "Audit codebase for SQL injection and fix all instances with security tests",
        signals: &["security", "audit", "sqli"],
    },
    TierExemplar {
        id: "T4-02",
        tier: ComplexityTier::T4,
        prompt: "Zero-downtime migration from Postgres 14 to 16 for production database",
        signals: &["migrate", "database", "production"],
    },
    TierExemplar {
        id: "T4-03",
        tier: ComplexityTier::T4,
        prompt: "Redesign auth for SOC 2 with MFA for admins and audit log",
        signals: &["security", "soc2", "auth"],
    },
    TierExemplar {
        id: "T4-04",
        tier: ComplexityTier::T4,
        prompt: "Respond to credential stuffing with rate limits CAPTCHA and anomaly alerts",
        signals: &["security", "incident", "auth"],
    },
    TierExemplar {
        id: "T4-05",
        tier: ComplexityTier::T4,
        prompt: "Implement HIPAA-grade encryption at rest with key rotation plan",
        signals: &["security", "hipaa", "encryption"],
    },
    TierExemplar {
        id: "T4-06",
        tier: ComplexityTier::T4,
        prompt: "Migrate Stripe Charges to PaymentIntents without double-charging subscribers",
        signals: &["migrate", "stripe", "billing"],
    },
    TierExemplar {
        id: "T4-07",
        tier: ComplexityTier::T4,
        prompt: "Extract reporting service from monolith with stable contracts",
        signals: &["architecture", "microservice", "migrate"],
    },
    TierExemplar {
        id: "T4-08",
        tier: ComplexityTier::T4,
        prompt: "Post-incident contain exposed S3 bucket rotate keys patch IAM",
        signals: &["incident", "security", "remediation"],
    },
    TierExemplar {
        id: "T4-09",
        tier: ComplexityTier::T4,
        prompt: "Ship API v2 with breaking changes versioning and v1 sunset plan",
        signals: &["api", "breaking", "migration"],
    },
    TierExemplar {
        id: "T4-10",
        tier: ComplexityTier::T4,
        prompt: "Harden Solidity contracts before mainnet reentrancy and access control",
        signals: &["security", "web3", "audit"],
    },
    TierExemplar {
        id: "T4-11",
        tier: ComplexityTier::T4,
        prompt: "Disaster recovery runbook and automate failover RTO under 15 minutes",
        signals: &["disaster-recovery", "infra", "production"],
    },
    TierExemplar {
        id: "T4-12",
        tier: ComplexityTier::T4,
        prompt: "Requirements unclear propose three architectures for real-time collaboration",
        signals: &["architecture", "ambiguous", "design"],
    },
];
