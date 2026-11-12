use super::InterruptedActiveGoal;
use super::inherit_thread_goal_snapshot;
use codex_protocol::ThreadId;
use codex_state::GoalUpdate;
use codex_state::StateRuntime;
use codex_state::ThreadGoal;
use codex_state::ThreadGoalStatus;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tempfile::TempDir;

const TURN_ID: &str = "turn-1";

struct Fixture {
    state_db: Arc<StateRuntime>,
    source: ThreadId,
    target: ThreadId,
    _home: TempDir,
}

impl Fixture {
    async fn new() -> anyhow::Result<Self> {
        let home = TempDir::new()?;
        let state_db = StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(home.path().abs()),
            "test-provider".to_string(),
        )
        .await?;
        Ok(Self {
            state_db,
            source: ThreadId::new(),
            target: ThreadId::new(),
            _home: home,
        })
    }

    async fn set_goal(&self, status: ThreadGoalStatus) -> anyhow::Result<ThreadGoal> {
        self.state_db
            .thread_goals()
            .replace_thread_goal(
                self.source,
                "keep going",
                status,
                /*token_budget*/ None,
            )
            .await
    }

    async fn claim(&self, turn_id: &str) -> anyhow::Result<Option<InterruptedActiveGoal>> {
        InterruptedActiveGoal::capture(&self.state_db, self.source, turn_id).await
    }

    async fn interrupt_pause(&self, goal: &ThreadGoal, turn_id: &str) -> anyhow::Result<()> {
        self.state_db
            .thread_goals()
            .pause_active_thread_goal_for_interrupted_turn(self.source, &goal.goal_id, turn_id)
            .await?
            .expect("active goal should be paused");
        Ok(())
    }

    /// Inherits under `claim` and returns the source and target goals afterwards.
    async fn inherit(
        &self,
        claim: Option<&InterruptedActiveGoal>,
    ) -> anyhow::Result<(ThreadGoal, ThreadGoal)> {
        assert!(
            inherit_thread_goal_snapshot(&self.state_db, self.source, self.target, claim).await?
        );
        let goals = self.state_db.thread_goals();
        Ok((
            goals
                .get_thread_goal(self.source)
                .await?
                .expect("source goal"),
            goals
                .get_thread_goal(self.target)
                .await?
                .expect("target goal"),
        ))
    }
}

fn snapshot_of(source: &ThreadGoal, target: ThreadId) -> ThreadGoal {
    ThreadGoal {
        thread_id: target,
        ..source.clone()
    }
}

#[tokio::test]
async fn goal_paused_by_the_owned_interrupt_is_inherited_as_active() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let goal = fixture.set_goal(ThreadGoalStatus::Active).await?;
    let claim = fixture.claim(TURN_ID).await?.expect("active goal claim");
    fixture.interrupt_pause(&goal, TURN_ID).await?;

    let (source, target) = fixture.inherit(claim.as_ref()).await?;

    assert_eq!(ThreadGoalStatus::Paused, source.status);
    assert_eq!(
        ThreadGoal {
            status: ThreadGoalStatus::Active,
            ..snapshot_of(&source, fixture.target)
        },
        target
    );
    Ok(())
}

#[tokio::test]
async fn goals_that_are_not_active_make_no_claim() -> anyhow::Result<()> {
    for status in [
        ThreadGoalStatus::Paused,
        ThreadGoalStatus::Blocked,
        ThreadGoalStatus::Complete,
    ] {
        let fixture = Fixture::new().await?;
        fixture.set_goal(status).await?;

        assert!(fixture.claim(TURN_ID).await?.is_none(), "{status:?}");
        let (source, target) = fixture.inherit(/*claim*/ None).await?;
        assert_eq!(snapshot_of(&source, fixture.target), target, "{status:?}");
    }
    Ok(())
}

#[tokio::test]
async fn a_write_after_the_interrupt_pause_inherits_the_unchanged_snapshot() -> anyhow::Result<()> {
    enum Write {
        SameStatusPause,
        ObjectiveEdit,
        Block,
        Complete,
        Replace,
    }

    for write in [
        Write::SameStatusPause,
        Write::ObjectiveEdit,
        Write::Block,
        Write::Complete,
        Write::Replace,
    ] {
        let fixture = Fixture::new().await?;
        let goal = fixture.set_goal(ThreadGoalStatus::Active).await?;
        let claim = fixture.claim(TURN_ID).await?.expect("active goal claim");
        fixture.interrupt_pause(&goal, TURN_ID).await?;
        let goals = fixture.state_db.thread_goals();
        let update = |status, objective: Option<&str>| GoalUpdate {
            objective: objective.map(str::to_string),
            status,
            token_budget: None,
            expected_goal_id: Some(goal.goal_id.clone()),
        };
        match write {
            Write::SameStatusPause => {
                goals
                    .update_thread_goal(
                        fixture.source,
                        update(Some(ThreadGoalStatus::Paused), /*objective*/ None),
                    )
                    .await?;
            }
            Write::ObjectiveEdit => {
                goals
                    .update_thread_goal(
                        fixture.source,
                        update(/*status*/ None, Some("edited objective")),
                    )
                    .await?;
            }
            Write::Block => {
                goals
                    .update_thread_goal(
                        fixture.source,
                        update(Some(ThreadGoalStatus::Blocked), /*objective*/ None),
                    )
                    .await?;
            }
            Write::Complete => {
                goals
                    .update_thread_goal(
                        fixture.source,
                        update(Some(ThreadGoalStatus::Complete), /*objective*/ None),
                    )
                    .await?;
            }
            Write::Replace => {
                fixture.set_goal(ThreadGoalStatus::Active).await?;
            }
        }

        let (source, target) = fixture.inherit(Some(&claim)).await?;

        assert_eq!(snapshot_of(&source, fixture.target), target);
    }
    Ok(())
}

#[tokio::test]
async fn cleared_goal_is_not_inherited() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let goal = fixture.set_goal(ThreadGoalStatus::Active).await?;
    let claim = fixture.claim(TURN_ID).await?.expect("active goal claim");
    fixture.interrupt_pause(&goal, TURN_ID).await?;
    fixture
        .state_db
        .thread_goals()
        .delete_thread_goal(fixture.source)
        .await?;

    assert!(
        !inherit_thread_goal_snapshot(
            &fixture.state_db,
            fixture.source,
            fixture.target,
            Some(&claim)
        )
        .await?
    );
    assert_eq!(
        None,
        fixture
            .state_db
            .thread_goals()
            .get_thread_goal(fixture.target)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn pause_by_another_turns_interrupt_is_not_reactivated() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let goal = fixture.set_goal(ThreadGoalStatus::Active).await?;
    let claim = fixture.claim("turn-2").await?.expect("active goal claim");
    fixture.interrupt_pause(&goal, TURN_ID).await?;

    let (source, target) = fixture.inherit(Some(&claim)).await?;

    assert_eq!(ThreadGoalStatus::Paused, source.status);
    assert_eq!(snapshot_of(&source, fixture.target), target);
    Ok(())
}
