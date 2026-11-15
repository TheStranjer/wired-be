-- The poll-end notification flag (WBE-113): whether the `polls.end_notify`
-- job has already told the poll's author and its local voters that the poll
-- closed. Pleroma schedules one `PollWorker` Oban job per expiring poll
-- (`lib/pleroma/workers/poll_worker.ex`) and that job runs once; this
-- instance finds closed polls by sweep instead, so the row itself records
-- that it was swept, and the next pass walks past it.
ALTER TABLE polls
    ADD COLUMN ended_notified boolean NOT NULL DEFAULT false;
