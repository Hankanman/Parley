-- MeetingsRepository::get_meetings orders by created_at DESC (issue #53);
-- index it so that scan stays cheap as the meetings table grows.
CREATE INDEX IF NOT EXISTS idx_meetings_created_at ON meetings(created_at DESC);
