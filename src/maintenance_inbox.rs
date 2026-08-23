//! Read-only view model for the dashboard's maintenance inbox.
//!
//! It deliberately exposes no lifecycle mutation. Proposal generation and
//! semantic review stay with an external agent or human, while apply, revert,
//! reject, and finalize remain explicit CLI operations.

use crate::config::Config;
use crate::{maintenance, paths, staging};

const MAX_PREVIEW_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Good,
    Warning,
    Error,
    Accent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailLine {
    pub text: String,
    pub tone: Tone,
}

impl DetailLine {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailDocument {
    pub title: String,
    pub lines: Vec<DetailLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRow {
    pub id: String,
    pub badge: String,
    pub summary: String,
    pub tone: Tone,
}

#[derive(Debug, Clone)]
enum Record {
    Candidate(staging::Candidate),
    Proposal(maintenance::ProposalSummary),
}

impl Record {
    fn id(&self) -> &str {
        match self {
            Self::Candidate(candidate) => &candidate.id,
            Self::Proposal(proposal) => &proposal.id,
        }
    }

    fn timestamp(&self) -> i64 {
        match self {
            Self::Candidate(candidate) => candidate.ts,
            Self::Proposal(proposal) => proposal.created_at,
        }
    }

    fn row(&self) -> InboxRow {
        match self {
            Self::Candidate(candidate) => InboxRow {
                id: candidate.id.clone(),
                badge: "candidate".to_string(),
                summary: format!("{} · {}", candidate.reason, candidate.kind),
                tone: Tone::Warning,
            },
            Self::Proposal(proposal) => InboxRow {
                id: proposal.id.clone(),
                badge: proposal.state.clone(),
                summary: format!(
                    "{} · {}",
                    enum_name(proposal.transition),
                    proposal.target.as_deref().unwrap_or("no memory write")
                ),
                tone: state_tone(&proposal.state),
            },
        }
    }
}

#[derive(Debug)]
pub struct Inbox {
    records: Vec<Record>,
    selected: usize,
    pub detail_scroll: u16,
    pub detail: DetailDocument,
    pub status: String,
}

impl Inbox {
    pub fn load(cfg: &Config, can_verify_locally: bool) -> Self {
        let mut inbox = Self {
            records: Vec::new(),
            selected: 0,
            detail_scroll: 0,
            detail: empty_detail(),
            status: String::new(),
        };
        inbox.refresh(cfg, can_verify_locally);
        inbox
    }

    pub fn refresh(&mut self, cfg: &Config, can_verify_locally: bool) {
        let selected_id = self
            .records
            .get(self.selected)
            .map(Record::id)
            .map(str::to_string);
        let candidate_dir = paths::staging_dir(&cfg.brain_root);
        let mut records: Vec<Record> = staging::list(&candidate_dir)
            .into_iter()
            .map(Record::Candidate)
            .chain(maintenance::list(cfg).into_iter().map(Record::Proposal))
            .collect();
        records.sort_by(|a, b| {
            b.timestamp()
                .cmp(&a.timestamp())
                .then_with(|| a.id().cmp(b.id()))
        });
        self.records = records;
        self.selected = selected_id
            .as_deref()
            .and_then(|id| self.records.iter().position(|record| record.id() == id))
            .unwrap_or(0)
            .min(self.records.len().saturating_sub(1));
        self.detail_scroll = 0;
        self.status = format!(
            "{} maintenance item(s) · read-only inbox",
            self.records.len()
        );
        self.load_selected(cfg, can_verify_locally);
    }

    pub fn rows(&self) -> Vec<InboxRow> {
        self.records.iter().map(Record::row).collect()
    }

    pub fn selected(&self) -> Option<usize> {
        (!self.records.is_empty()).then_some(self.selected)
    }

    pub fn select_next(&mut self, cfg: &Config, can_verify_locally: bool) {
        if self.selected + 1 < self.records.len() {
            self.selected += 1;
            self.detail_scroll = 0;
            self.load_selected(cfg, can_verify_locally);
        }
    }

    pub fn select_previous(&mut self, cfg: &Config, can_verify_locally: bool) {
        if self.selected > 0 {
            self.selected -= 1;
            self.detail_scroll = 0;
            self.load_selected(cfg, can_verify_locally);
        }
    }

    pub fn select_first(&mut self, cfg: &Config, can_verify_locally: bool) {
        if !self.records.is_empty() {
            self.selected = 0;
            self.detail_scroll = 0;
            self.load_selected(cfg, can_verify_locally);
        }
    }

    pub fn select_last(&mut self, cfg: &Config, can_verify_locally: bool) {
        if !self.records.is_empty() {
            self.selected = self.records.len() - 1;
            self.detail_scroll = 0;
            self.load_selected(cfg, can_verify_locally);
        }
    }

    pub fn scroll_down(&mut self, lines: u16) {
        self.detail_scroll = self.detail_scroll.saturating_add(lines);
    }

    pub fn scroll_up(&mut self, lines: u16) {
        self.detail_scroll = self.detail_scroll.saturating_sub(lines);
    }

    fn load_selected(&mut self, cfg: &Config, can_verify_locally: bool) {
        self.detail = match self.records.get(self.selected) {
            Some(Record::Candidate(candidate)) => candidate_detail(candidate),
            Some(Record::Proposal(summary)) => proposal_detail(cfg, summary, can_verify_locally)
                .unwrap_or_else(|error| {
                    error_detail(
                        &summary.id,
                        format!("could not inspect proposal: {error:#}"),
                    )
                }),
            None => empty_detail(),
        };
    }
}

fn enum_name(value: impl std::fmt::Debug) -> String {
    format!("{value:?}").to_ascii_lowercase()
}

fn state_tone(state: &str) -> Tone {
    match state {
        "pending" | "applied" => Tone::Warning,
        "finalized" => Tone::Good,
        "rejected" | "reverted" => Tone::Muted,
        _ => Tone::Error,
    }
}

fn empty_detail() -> DetailDocument {
    DetailDocument {
        title: " maintenance inbox ".to_string(),
        lines: vec![
            DetailLine::new(
                "No candidates or proposals are waiting in the tree.",
                Tone::Muted,
            ),
            DetailLine::new(
                "Deterministic traps will place new ring-5 candidates here.",
                Tone::Muted,
            ),
        ],
    }
}

fn error_detail(id: &str, error: String) -> DetailDocument {
    DetailDocument {
        title: format!(" {id} "),
        lines: vec![DetailLine::new(error, Tone::Error)],
    }
}

fn heading(lines: &mut Vec<DetailLine>, text: &str) {
    if !lines.is_empty() {
        lines.push(DetailLine::new("", Tone::Normal));
    }
    lines.push(DetailLine::new(text, Tone::Accent));
}

fn fields(lines: &mut Vec<DetailLine>, values: impl IntoIterator<Item = (String, String)>) {
    lines.extend(
        values
            .into_iter()
            .map(|(name, value)| DetailLine::new(format!("{name}: {value}"), Tone::Normal)),
    );
}

fn lifecycle(state: &str) -> String {
    match state {
        "candidate" => "[CANDIDATE] → pending → applied → finalized".to_string(),
        "pending" => "candidate → [PENDING] → applied → finalized".to_string(),
        "applied" => "candidate → pending → [APPLIED] → finalized".to_string(),
        "finalized" => "candidate → pending → applied → [FINALIZED]".to_string(),
        "rejected" => "candidate → pending → [REJECTED]".to_string(),
        "reverted" => "candidate → pending → applied → [REVERTED]".to_string(),
        other => format!("candidate → [{other}]"),
    }
}

fn candidate_detail(candidate: &staging::Candidate) -> DetailDocument {
    let mut lines = Vec::new();
    lines.push(DetailLine::new(lifecycle("candidate"), Tone::Warning));
    heading(&mut lines, "Captured candidate");
    fields(
        &mut lines,
        [
            ("State".to_string(), "CANDIDATE".to_string()),
            ("Transition".to_string(), "not proposed".to_string()),
            ("Target".to_string(), "not selected".to_string()),
            ("Trap".to_string(), candidate.reason.clone()),
            ("Event kind".to_string(), candidate.kind.clone()),
            (
                "Recorded".to_string(),
                format!("Unix timestamp {}", candidate.ts),
            ),
            ("Origin".to_string(), candidate.host.clone()),
            ("Session".to_string(), candidate.session.clone()),
        ],
    );
    heading(&mut lines, "Captured evidence preview");
    let payload = serde_json::to_string_pretty(&candidate.payload)
        .unwrap_or_else(|_| "<unreadable payload>".to_string());
    lines.extend(
        payload
            .lines()
            .map(|line| DetailLine::new(line, Tone::Normal)),
    );
    heading(&mut lines, "Next step");
    lines.push(DetailLine::new(
        format!("cfetch maintain packet {} --json", candidate.id),
        Tone::Accent,
    ));
    lines.push(DetailLine::new(
        "A connected agent can request the same bounded packet with cfetch_maintenance_packet.",
        Tone::Muted,
    ));
    lines.push(DetailLine::new(
        "Proposal generation and semantic review remain external; this dashboard never calls a model.",
        Tone::Muted,
    ));
    DetailDocument {
        title: format!(" candidate · {} ", candidate.id),
        lines,
    }
}

fn proposal_detail(
    cfg: &Config,
    summary: &maintenance::ProposalSummary,
    can_verify_locally: bool,
) -> anyhow::Result<DetailDocument> {
    let (state, proposal) = maintenance::get(cfg, &summary.id)?;
    let review = maintenance::get_review(cfg, &summary.id)?;
    let verification = if can_verify_locally {
        maintenance::inspect_verification(cfg, &summary.id)?
    } else {
        None
    };

    let mut lines = Vec::new();
    lines.push(DetailLine::new(lifecycle(&state), state_tone(&state)));
    heading(&mut lines, "Proposal");
    fields(
        &mut lines,
        [
            ("State".to_string(), state.to_ascii_uppercase()),
            ("Transition".to_string(), enum_name(proposal.transition)),
            ("Authority".to_string(), enum_name(proposal.authority)),
            (
                "Target".to_string(),
                proposal
                    .target
                    .clone()
                    .unwrap_or_else(|| "no memory write".to_string()),
            ),
            (
                "Created".to_string(),
                format!("Unix timestamp {}", proposal.created_at),
            ),
        ],
    );
    heading(&mut lines, "Candidate evidence");
    if proposal.candidates.is_empty() {
        lines.push(DetailLine::new("No candidates recorded.", Tone::Error));
    } else {
        lines.extend(proposal.candidates.iter().map(|candidate| {
            DetailLine::new(
                format!(
                    "• {}  sha256:{}",
                    candidate.id,
                    short_hash(&candidate.sha256)
                ),
                Tone::Normal,
            )
        }));
    }
    for evidence in &proposal.evidence {
        lines.push(DetailLine::new(
            format!("• evidence {evidence}"),
            Tone::Normal,
        ));
    }
    for citation in &proposal.related_citations {
        lines.push(DetailLine::new(
            format!(
                "• cite {}  sha256:{}",
                citation.cite,
                short_hash(&citation.sha256)
            ),
            Tone::Normal,
        ));
    }
    heading(&mut lines, "Rationale");
    lines.extend(
        proposal
            .rationale
            .lines()
            .map(|line| DetailLine::new(line, Tone::Normal)),
    );

    review_section(&mut lines, review.as_ref());
    verification_section(
        &mut lines,
        &state,
        verification.as_ref(),
        can_verify_locally,
    );
    diff_section(&mut lines, &proposal, verification.as_ref());
    next_step_section(
        &mut lines,
        &state,
        &proposal,
        review.as_ref(),
        verification.as_ref(),
    );

    Ok(DetailDocument {
        title: format!(" {} · {} ", state, proposal.id),
        lines,
    })
}

fn review_section(lines: &mut Vec<DetailLine>, review: Option<&maintenance::Review>) {
    heading(lines, "Independent semantic review");
    let Some(review) = review else {
        lines.push(DetailLine::new("○ not recorded", Tone::Warning));
        for gate in [
            "evidence coverage",
            "factual faithfulness",
            "preservation",
            "authority fit",
            "target fit",
            "contradiction check",
        ] {
            lines.push(DetailLine::new(format!("○ {gate}"), Tone::Muted));
        }
        return;
    };
    let passed = review.verdict == maintenance::ReviewVerdict::Pass;
    lines.push(DetailLine::new(
        format!(
            "{} verdict {} via {} · {}",
            mark(passed),
            enum_name(review.verdict),
            enum_name(review.method),
            review.id
        ),
        bool_tone(passed),
    ));
    for (name, ok) in [
        ("evidence coverage", review.evidence_coverage),
        ("factual faithfulness", review.factual_faithfulness),
        ("preservation", review.preservation),
        ("authority fit", review.authority_fit),
        ("target fit", review.target_fit),
        ("contradiction check", review.contradiction_checked),
    ] {
        lines.push(DetailLine::new(
            format!("{} {name}", mark(ok)),
            bool_tone(ok),
        ));
    }
    lines.push(DetailLine::new(
        format!("Notes: {}", review.notes),
        Tone::Muted,
    ));
}

fn verification_section(
    lines: &mut Vec<DetailLine>,
    state: &str,
    verification: Option<&maintenance::Verification>,
    can_verify_locally: bool,
) {
    heading(lines, "Deterministic verification");
    if !can_verify_locally && matches!(state, "pending" | "applied") {
        lines.push(DetailLine::new(
            "Not run here: this host delegates queries and the dashboard will not create a second local index.",
            Tone::Warning,
        ));
        lines.push(DetailLine::new(
            "Run verification on a storage host that holds the tree and its current derived state.",
            Tone::Muted,
        ));
        return;
    }
    let Some(verification) = verification else {
        lines.push(DetailLine::new(
            format!("Not rerun: {state} is a terminal lifecycle record."),
            Tone::Muted,
        ));
        return;
    };
    lines.push(DetailLine::new(
        if verification.valid {
            "READY: every current gate passes"
        } else {
            "BLOCKED: one or more gates fail"
        },
        bool_tone(verification.valid),
    ));
    for check in &verification.checks {
        lines.push(DetailLine::new(
            format!("{} {} — {}", mark(check.ok), check.name, check.detail),
            bool_tone(check.ok),
        ));
    }
}

fn diff_section(
    lines: &mut Vec<DetailLine>,
    proposal: &maintenance::Proposal,
    verification: Option<&maintenance::Verification>,
) {
    let diff = verification
        .and_then(|report| report.diff.clone())
        .or_else(|| maintenance::exact_diff(proposal));
    let Some(diff) = diff else {
        heading(lines, "Memory write");
        lines.push(DetailLine::new(
            "None. This decision-only transition cannot alter trusted memory.",
            Tone::Good,
        ));
        return;
    };
    heading(lines, "Exact proposed diff");
    let (preview, omitted) = bounded_preview(&diff, MAX_PREVIEW_BYTES);
    lines.extend(preview.lines().map(|line| {
        let tone = if line.starts_with('+') && !line.starts_with("+++") {
            Tone::Good
        } else if line.starts_with('-') && !line.starts_with("---") {
            Tone::Error
        } else {
            Tone::Normal
        };
        DetailLine::new(line, tone)
    }));
    if omitted > 0 {
        lines.push(DetailLine::new(
            format!(
                "… {omitted} diff bytes omitted from this preview; `cfetch maintain show {} --json` retains the exact complete before/after bytes.",
                proposal.id
            ),
            Tone::Warning,
        ));
    }
}

fn next_step_section(
    lines: &mut Vec<DetailLine>,
    state: &str,
    proposal: &maintenance::Proposal,
    review: Option<&maintenance::Review>,
    verification: Option<&maintenance::Verification>,
) {
    heading(lines, "Next explicit command");
    match state {
        "pending" if review.is_none() => lines.push(DetailLine::new(
            format!("cfetch maintain review {} --file review.json", proposal.id),
            Tone::Accent,
        )),
        "pending"
            if review.is_some_and(|review| {
                review.verdict == maintenance::ReviewVerdict::Fail
                    || !review.evidence_coverage
                    || !review.factual_faithfulness
                    || !review.preservation
                    || !review.authority_fit
                    || !review.target_fit
                    || !review.contradiction_checked
            }) =>
        {
            lines.push(DetailLine::new(
                "The failed review is immutable. Revise the proposal to produce a new id.",
                Tone::Error,
            ));
            lines.push(DetailLine::new(
                format!("cfetch maintain reject {}", proposal.id),
                Tone::Accent,
            ));
        }
        "pending" => {
            if let Some(token) = verification
                .filter(|report| report.valid)
                .and_then(|report| report.approval_token.as_deref())
            {
                lines.push(DetailLine::new(
                    format!(
                        "cfetch maintain apply {} --approval-token {}",
                        proposal.id, token
                    ),
                    Tone::Accent,
                ));
                lines.push(DetailLine::new(
                    "Copying this exact, revision-bound token is the approval act; the inbox never applies automatically.",
                    Tone::Muted,
                ));
            } else {
                lines.push(DetailLine::new(
                    format!("cfetch maintain verify {}", proposal.id),
                    Tone::Accent,
                ));
            }
        }
        "applied" => {
            if let Some(target) = proposal.target.as_deref() {
                lines.push(DetailLine::new(
                    format!("Commit the exact bytes in {target}, then:"),
                    Tone::Normal,
                ));
            }
            lines.push(DetailLine::new(
                format!("cfetch maintain finalize {}", proposal.id),
                Tone::Accent,
            ));
            lines.push(DetailLine::new(
                format!(
                    "Undo before finalization: cfetch maintain revert {}",
                    proposal.id
                ),
                Tone::Warning,
            ));
        }
        "finalized" => lines.push(DetailLine::new(
            "Complete. Git contains the approved bytes and the candidate evidence is settled.",
            Tone::Good,
        )),
        "rejected" => lines.push(DetailLine::new(
            "Closed without applying the proposal.",
            Tone::Muted,
        )),
        "reverted" => lines.push(DetailLine::new(
            "Closed after restoring the captured before bytes.",
            Tone::Muted,
        )),
        _ => lines.push(DetailLine::new(
            "No action is available for this state.",
            Tone::Muted,
        )),
    }
}

fn mark(ok: bool) -> &'static str {
    if ok { "✓" } else { "✗" }
}

fn bool_tone(ok: bool) -> Tone {
    if ok { Tone::Good } else { Tone::Error }
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn bounded_preview(text: &str, limit: usize) -> (String, usize) {
    if text.len() <= limit {
        return (text.to_string(), 0);
    }
    let head_target = limit * 3 / 4;
    let tail_target = limit - head_target;
    let mut head = head_target.min(text.len());
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(tail_target);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let omitted = tail.saturating_sub(head);
    (
        format!("{}\n… preview gap …\n{}", &text[..head], &text[tail..]),
        omitted,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(id: &str, ts: i64) -> staging::Candidate {
        staging::Candidate {
            id: id.to_string(),
            reason: "fix-discovered".to_string(),
            session: "session-a".to_string(),
            host: "example-host".to_string(),
            ts,
            kind: "bash".to_string(),
            payload: json!({"command": "cargo test", "recovered": true}),
        }
    }

    fn text(document: &DetailDocument) -> String {
        document
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn candidate_detail_exposes_evidence_and_external_next_step() {
        let document = candidate_detail(&candidate("fix-discovered-a1b2c3d4", 42));
        let rendered = text(&document);
        assert!(rendered.contains("[CANDIDATE] → pending → applied → finalized"));
        assert!(
            rendered.contains("cargo test"),
            "captured payload must be visible"
        );
        assert!(rendered.contains("cfetch maintain packet fix-discovered-a1b2c3d4 --json"));
        assert!(rendered.contains("never calls a model"));
    }

    #[test]
    fn inbox_combines_candidates_and_all_proposal_states_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config {
            brain_root: root.path().to_path_buf(),
            ..Config::default()
        };
        let staging_dir = paths::staging_dir(root.path());
        staging::write(&staging_dir, &candidate("fix-discovered-a1b2c3d4", 42)).unwrap();
        let finalized = root
            .path()
            .join("todo/staging/maintenance/finalized/maintenance-000000000000.md");
        std::fs::create_dir_all(finalized.parent().unwrap()).unwrap();
        let proposal = maintenance::Proposal {
            schema_version: maintenance::SCHEMA_VERSION,
            id: "maintenance-000000000000".to_string(),
            created_at: 41,
            created_by_host: "example-host".to_string(),
            candidates: Vec::new(),
            transition: maintenance::Transition::Noop,
            target: None,
            authority: maintenance::Authority::Unendorsed,
            valid_until: None,
            rationale: "Nothing durable to record.".to_string(),
            evidence: Vec::new(),
            related_citations: Vec::new(),
            before_sha256: None,
            before: None,
            after: None,
        };
        let json = serde_json::to_string_pretty(&proposal).unwrap();
        std::fs::write(
            finalized,
            format!("---\nring: 5\n---\n\n```json\n{json}\n```\n"),
        )
        .unwrap();

        let inbox = Inbox::load(&cfg, true);
        let rows = inbox.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].badge, "candidate");
        assert_eq!(rows[1].badge, "finalized");
        assert!(text(&inbox.detail).contains("Captured evidence preview"));
    }

    #[test]
    fn review_section_names_every_semantic_gate_and_immutable_failure() {
        let review = maintenance::Review {
            schema_version: maintenance::SCHEMA_VERSION,
            id: "review-example".to_string(),
            proposal_id: "maintenance-example".to_string(),
            proposal_sha256: "00".repeat(32),
            created_at: 1,
            created_by_host: "example-host".to_string(),
            verdict: maintenance::ReviewVerdict::Fail,
            method: maintenance::ReviewMethod::IndependentAgent,
            evidence_coverage: true,
            factual_faithfulness: false,
            preservation: true,
            authority_fit: true,
            target_fit: false,
            contradiction_checked: true,
            notes: "The target overstates the evidence.".to_string(),
        };
        let mut lines = Vec::new();
        review_section(&mut lines, Some(&review));
        let rendered = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for gate in [
            "evidence coverage",
            "factual faithfulness",
            "preservation",
            "authority fit",
            "target fit",
            "contradiction check",
        ] {
            assert!(rendered.contains(gate), "missing {gate}: {rendered}");
        }
        assert!(rendered.contains("✗ verdict fail"));
    }

    #[test]
    fn remote_verification_is_explicitly_skipped() {
        let mut lines = Vec::new();
        verification_section(&mut lines, "pending", None, false);
        let rendered = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("delegates queries"));
        assert!(rendered.contains("will not create a second local index"));
        assert!(rendered.contains("storage host"));
        assert!(!rendered.contains("READY"));
    }

    #[test]
    fn valid_pending_proposal_exposes_only_the_revision_bound_apply_command() {
        let proposal = maintenance::Proposal {
            schema_version: maintenance::SCHEMA_VERSION,
            id: "maintenance-001122334455".to_string(),
            created_at: 1,
            created_by_host: "example-host".to_string(),
            candidates: Vec::new(),
            transition: maintenance::Transition::Noop,
            target: None,
            authority: maintenance::Authority::Unendorsed,
            valid_until: None,
            rationale: "Nothing durable to record.".to_string(),
            evidence: Vec::new(),
            related_citations: Vec::new(),
            before_sha256: None,
            before: None,
            after: None,
        };
        let review = maintenance::Review {
            schema_version: maintenance::SCHEMA_VERSION,
            id: "review-example".to_string(),
            proposal_id: proposal.id.clone(),
            proposal_sha256: "00".repeat(32),
            created_at: 1,
            created_by_host: "example-host".to_string(),
            verdict: maintenance::ReviewVerdict::Pass,
            method: maintenance::ReviewMethod::IndependentAgent,
            evidence_coverage: true,
            factual_faithfulness: true,
            preservation: true,
            authority_fit: true,
            target_fit: true,
            contradiction_checked: true,
            notes: "All gates pass.".to_string(),
        };
        let verification = maintenance::Verification {
            proposal_id: proposal.id.clone(),
            valid: true,
            checks: Vec::new(),
            review_id: Some(review.id.clone()),
            approval_token: Some("approve-revision001122".to_string()),
            diff: None,
        };
        let mut lines = Vec::new();
        next_step_section(
            &mut lines,
            "pending",
            &proposal,
            Some(&review),
            Some(&verification),
        );
        let rendered = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(
            "cfetch maintain apply maintenance-001122334455 --approval-token approve-revision001122"
        ));
        assert!(rendered.contains("exact, revision-bound token is the approval act"));
        assert!(!rendered.contains("maintain verify"));
    }

    #[test]
    fn large_diff_preview_is_bounded_and_keeps_both_ends() {
        let input = format!("START{}END", "x".repeat(400));
        let (preview, omitted) = bounded_preview(&input, 100);
        assert!(preview.starts_with("START"));
        assert!(preview.ends_with("END"));
        assert!(preview.contains("preview gap"));
        assert!(omitted > 0);
    }
}
