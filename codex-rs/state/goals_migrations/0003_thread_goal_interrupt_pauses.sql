-- Records that a goal was paused by the interrupt of one specific turn. Every
-- other write to the goal row, including same-status and same-clock writes,
-- invalidates the record, so it only survives while the pause is untouched.
CREATE TABLE thread_goal_interrupt_pauses (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    turn_id TEXT NOT NULL
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
