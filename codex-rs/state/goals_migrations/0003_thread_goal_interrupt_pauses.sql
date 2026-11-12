-- Ownership record for a goal that a fork interrupts together with its running turn.
--
-- A fork first claims the goal that is active right now ('claimed'). The interrupt's pause
-- then turns the claim into 'paused' in the same transaction. Every other insert, update, or
-- delete of the goal row, including same-status writes in the same clock tick, deletes the
-- record, so it survives only while no one else has touched the goal since the claim.
CREATE TABLE thread_goal_interrupt_pauses (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    phase TEXT NOT NULL CHECK(phase IN ('claimed', 'paused'))
);

CREATE TRIGGER thread_goal_interrupt_pauses_clear_on_insert
AFTER INSERT ON thread_goals
BEGIN
    DELETE FROM thread_goal_interrupt_pauses WHERE thread_id = NEW.thread_id;
END;

CREATE TRIGGER thread_goal_interrupt_pauses_clear_on_update
AFTER UPDATE ON thread_goals
BEGIN
    DELETE FROM thread_goal_interrupt_pauses WHERE thread_id = NEW.thread_id;
END;

CREATE TRIGGER thread_goal_interrupt_pauses_clear_on_delete
AFTER DELETE ON thread_goals
BEGIN
    DELETE FROM thread_goal_interrupt_pauses WHERE thread_id = OLD.thread_id;
END;
