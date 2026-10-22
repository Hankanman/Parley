-- Demo data set for screenshots and docs: a fictional product team at
-- "Lumen Robotics". Every person, company and email here is made up.
-- Dates are relative to now, so the sidebar's Today / Yesterday groups
-- and the Upcoming list (next 12 hours) always look current.
-- Loaded by `cargo run -p parley-core --example seed_demo -- <data dir>`.

-- ---------------------------------------------------------------- settings

INSERT OR REPLACE INTO settings (id, provider, model, whisperModel)
VALUES ('1', 'builtin-ai', 'gemma3:4b', 'large-v3-turbo-q5_0');
INSERT OR REPLACE INTO transcript_settings (id, provider, model)
VALUES ('1', 'localWhisper', 'large-v3-turbo-q5_0');

INSERT OR REPLACE INTO app_settings (key, value, updated_at) VALUES
  ('onboarding_status', '{"version":"1.0","completed":true,"current_step":4,"model_status":{"parakeet":"downloaded","summary":"downloaded"},"last_updated":"2026-01-01T00:00:00+00:00"}', '2026-01-01T00:00:00+00:00'),
  ('theme_preference', '"dark"', '2026-01-01T00:00:00+00:00'),
  ('default_summary_template', '"standard_meeting"', '2026-01-01T00:00:00+00:00'),
  ('beta_features', '{"live_action_items":true}', '2026-01-01T00:00:00+00:00'),
  ('recording_preferences', '{"save_folder":"/home/alex/Documents/Parley Recordings","auto_save":true,"file_format":"mp4","preferred_mic_device":null,"preferred_system_device":null,"show_recording_notification":false,"auto_refine":true,"streaming_partials":true,"unload_model_after_recording":false,"offline_diarization_on_import":true,"meeting_autodetect":"notify","stop_when_call_ends":true}', '2026-01-01T00:00:00+00:00'),
  ('ui_config', '{"isAutoSummary":true,"primaryLanguage":"en","providerModelMap":{"builtin-ai":"gemma3:4b"},"showConfidenceIndicator":true}', '2026-01-01T00:00:00+00:00');

-- ---------------------------------------------------------------- speakers

INSERT INTO voice_profiles (id, name, email, embedding, embedding_dim, sample_count, is_self, created_at, updated_at) VALUES
  ('demo-vp-self',   'Alex Morgan',  'alex@lumen-robotics.example',  zeroblob(2048), 512, 6,  1, strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-40 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-40 days')),
  ('demo-vp-priya',  'Priya Shah',   'priya@lumen-robotics.example',  zeroblob(2048), 512, 24, 0, strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-35 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours')),
  ('demo-vp-marcus', 'Marcus Chen',  'marcus@lumen-robotics.example', zeroblob(2048), 512, 17, 0, strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-30 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours')),
  ('demo-vp-elena',  'Elena Novak',  'elena@lumen-robotics.example',  zeroblob(2048), 512, 11, 0, strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-21 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day')),
  ('demo-vp-sam',    'Sam Okafor',   'sam@harbor-logistics.example',  zeroblob(2048), 512, 5,  0, strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'),   strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'));

-- ---------------------------------------------------------------- calendar

INSERT INTO calendar_sources (id, url, label, last_fetched_at, created_at, updated_at) VALUES
  ('demo-cal', 'https://calendar.example.com/lumen-team.ics', 'Work', strftime('%Y-%m-%dT%H:%M:%S+00:00','now'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-40 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now'));

INSERT INTO calendar_events (id, source_id, ics_uid, summary, location, organizer_name, organizer_email, start_at, end_at, attendees_json, fetched_at) VALUES
  ('demo-ev-roadmap', 'demo-cal', 'roadmap@lumen', 'Q4 Roadmap Planning', 'Room 3 / Google Meet', 'Priya Shah', 'priya@lumen-robotics.example',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'),
    '[{"name":"Alex Morgan","email":"alex@lumen-robotics.example","role":"REQ-PARTICIPANT","status":"ACCEPTED","isOrganizer":false},{"name":"Priya Shah","email":"priya@lumen-robotics.example","role":"CHAIR","status":"ACCEPTED","isOrganizer":true},{"name":"Marcus Chen","email":"marcus@lumen-robotics.example","role":"REQ-PARTICIPANT","status":"ACCEPTED","isOrganizer":false},{"name":"Elena Novak","email":"elena@lumen-robotics.example","role":"REQ-PARTICIPANT","status":"ACCEPTED","isOrganizer":false}]',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now')),
  ('demo-ev-sprint', 'demo-cal', 'sprint@lumen', 'Sprint Planning', 'Room 3', 'Marcus Chen', 'marcus@lumen-robotics.example',
    strftime('%Y-%m-%dT%H:00:00+00:00','now','+2 hours'), strftime('%Y-%m-%dT%H:00:00+00:00','now','+3 hours'),
    '[{"name":"Alex Morgan","email":"alex@lumen-robotics.example","role":"REQ-PARTICIPANT","status":"ACCEPTED","isOrganizer":false},{"name":"Marcus Chen","email":"marcus@lumen-robotics.example","role":"CHAIR","status":"ACCEPTED","isOrganizer":true}]',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now')),
  ('demo-ev-harbor', 'demo-cal', 'harbor@lumen', 'Harbor Logistics: Pilot Kickoff', 'Zoom', 'Alex Morgan', 'alex@lumen-robotics.example',
    strftime('%Y-%m-%dT%H:00:00+00:00','now','+4 hours'), strftime('%Y-%m-%dT%H:30:00+00:00','now','+4 hours'),
    '[{"name":"Alex Morgan","email":"alex@lumen-robotics.example","role":"CHAIR","status":"ACCEPTED","isOrganizer":true},{"name":"Sam Okafor","email":"sam@harbor-logistics.example","role":"REQ-PARTICIPANT","status":"ACCEPTED","isOrganizer":false}]',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now'));

-- ---------------------------------------------------------------- meetings

INSERT INTO meetings (id, title, created_at, updated_at, completed_at, status, duration_seconds, calendar_event_id) VALUES
  ('demo-roadmap',  'Q4 Roadmap Planning',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), 'completed', 2834, 'demo-ev-roadmap'),
  ('demo-design',   'Design Review: Fleet Dashboard',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-5 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-5 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-5 hours'), 'completed', 1620, NULL),
  ('demo-harbor',   'Customer Call: Harbor Logistics',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+15 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+16 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+16 hours'), 'completed', 2410, NULL),
  ('demo-eng-sync', 'Weekly Engineering Sync',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+10 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+10 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+10 hours'), 'completed', 1795, NULL),
  ('demo-hiring',   'Hiring Debrief: Senior Firmware Engineer',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 days','start of day','+16 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 days','start of day','+16 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 days','start of day','+16 hours'), 'completed', 1310, NULL),
  ('demo-retro',    'Incident Retro: Telemetry Outage',
    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-6 days','start of day','+11 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-6 days','start of day','+11 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-6 days','start of day','+11 hours'), 'completed', 2105, NULL);

-- ---------------------------------------------------------------- transcripts

-- (meeting_id, sequence, start, end, speaker, voice profile, source, text)
CREATE TEMP TABLE seg (m TEXT, seq INTEGER, s REAL, e REAL, who TEXT, vp TEXT, src TEXT, txt TEXT);
INSERT INTO seg VALUES
  ('demo-roadmap', 1,   0.0,  14.2, 'Priya Shah',  'demo-vp-priya',  'system', 'Okay, let''s get started. The goal today is to leave with a ranked list for Q4 and a clear owner for each item.'),
  ('demo-roadmap', 2,  14.2,  31.6, 'Me',          NULL,             'microphone', 'Sounds good. I pulled the support data from last quarter, and offline mode is still the single biggest request from field teams.'),
  ('demo-roadmap', 3,  31.6,  52.0, 'Marcus Chen', 'demo-vp-marcus', 'system', 'That matches what we hear from the warehouse pilots. When the Wi-Fi drops, the robots keep working but the dashboard goes blank, and people panic.'),
  ('demo-roadmap', 4,  52.0,  71.4, 'Elena Novak', 'demo-vp-elena',  'system', 'From the design side we already have mockups for a degraded state. It''s mostly a question of how much telemetry we buffer on the device.'),
  ('demo-roadmap', 5,  71.4,  95.8, 'Marcus Chen', 'demo-vp-marcus', 'system', 'Buffering about thirty minutes is cheap. Anything past that and we need to revisit storage on the older units, which is a hardware conversation.'),
  ('demo-roadmap', 6,  95.8, 118.0, 'Priya Shah',  'demo-vp-priya',  'system', 'Let''s scope it to thirty minutes for Q4 then, and treat longer buffering as a stretch goal. Alex, can you own the offline mode spec?'),
  ('demo-roadmap', 7, 118.0, 129.5, 'Me',          NULL,             'microphone', 'Yes, I''ll have a first draft by Friday and share it with Elena for the UI states.'),
  ('demo-roadmap', 8, 129.5, 158.3, 'Priya Shah',  'demo-vp-priya',  'system', 'Second item is the fleet dashboard redesign. Elena, where are we with that?'),
  ('demo-roadmap', 9, 158.3, 189.0, 'Elena Novak', 'demo-vp-elena',  'system', 'The new layout tested well with five customers. The main open question is whether alerts live in a side panel or a separate page. I''d like one more round of testing before we commit.'),
  ('demo-roadmap',10, 189.0, 210.7, 'Me',          NULL,             'microphone', 'Could we run that test with Harbor Logistics? They''re starting the pilot next week and they have the busiest alert stream of anyone.'),
  ('demo-roadmap',11, 210.7, 226.2, 'Elena Novak', 'demo-vp-elena',  'system', 'That would be ideal. I''ll reach out to Sam and set up two sessions.'),
  ('demo-roadmap',12, 226.2, 255.9, 'Marcus Chen', 'demo-vp-marcus', 'system', 'One thing I want on the list is paying down the telemetry pipeline. After last week''s outage, I don''t think we can add more load to it without the queue rework.'),
  ('demo-roadmap',13, 255.9, 281.4, 'Priya Shah',  'demo-vp-priya',  'system', 'Agreed. Let''s make the queue rework a prerequisite for the dashboard launch rather than a separate line item. Marcus, how long?'),
  ('demo-roadmap',14, 281.4, 300.0, 'Marcus Chen', 'demo-vp-marcus', 'system', 'Three to four weeks with two engineers. I can have an estimate broken down by Wednesday.'),
  ('demo-roadmap',15, 300.0, 327.3, 'Me',          NULL,             'microphone', 'So the order is queue rework first, then dashboard, with offline mode running in parallel since it''s mostly device-side.'),
  ('demo-roadmap',16, 327.3, 352.8, 'Priya Shah',  'demo-vp-priya',  'system', 'Exactly. The last thing is the self-serve onboarding idea. I suggest we park it until Q1. We don''t have the people for it this quarter.'),
  ('demo-roadmap',17, 352.8, 366.0, 'Elena Novak', 'demo-vp-elena',  'system', 'Fine by me, as long as it''s first on the Q1 list.'),
  ('demo-roadmap',18, 366.0, 384.5, 'Priya Shah',  'demo-vp-priya',  'system', 'Noted. I''ll write up the ranked list and send it to the wider team by end of day. Thanks, everyone.'),

  ('demo-design', 1,   0.0,  18.4, 'Elena Novak', 'demo-vp-elena',  'system', 'I''ll share my screen. This is the new fleet overview, with every robot on one map and the alert rail on the right.'),
  ('demo-design', 2,  18.4,  37.9, 'Me',          NULL,             'microphone', 'This is much easier to scan. Can the rail collapse when there are no active alerts?'),
  ('demo-design', 3,  37.9,  58.0, 'Elena Novak', 'demo-vp-elena',  'system', 'Yes, it shrinks to a thin status strip. Red if anything needs attention, otherwise grey.'),
  ('demo-design', 4,  58.0,  80.6, 'Marcus Chen', 'demo-vp-marcus', 'system', 'The map needs to handle a thousand units. Clustering at low zoom levels would keep it fast.'),
  ('demo-design', 5,  80.6,  96.2, 'Elena Novak', 'demo-vp-elena',  'system', 'Good call, I''ll add clustering to the spec and mock the zoomed-out state.'),

  ('demo-harbor', 1,   0.0,  21.5, 'Sam Okafor',  'demo-vp-sam',    'system', 'Thanks for making time. We have forty units arriving at the Rotterdam site next week, and the team is excited.'),
  ('demo-harbor', 2,  21.5,  44.0, 'Me',          NULL,             'microphone', 'Great. What does success look like for you at the end of the pilot?'),
  ('demo-harbor', 3,  44.0,  73.8, 'Sam Okafor',  'demo-vp-sam',    'system', 'Two things: pick rates up by fifteen percent, and the night shift able to resolve alerts without calling an engineer.'),
  ('demo-harbor', 4,  73.8,  95.1, 'Me',          NULL,             'microphone', 'That''s very achievable. We''ll set up weekly check-ins and share a pick-rate report every Monday.'),
  ('demo-harbor', 5,  95.1, 112.6, 'Sam Okafor',  'demo-vp-sam',    'system', 'Perfect. Our main worry is the Wi-Fi in the cold storage area. It drops constantly.'),

  ('demo-eng-sync', 1,  0.0, 16.0, 'Marcus Chen', 'demo-vp-marcus', 'system', 'Quick round of updates. The firmware 4.2 release candidate is on the test fleet.'),
  ('demo-eng-sync', 2, 16.0, 35.5, 'Me',          NULL,             'microphone', 'Any regressions so far?'),
  ('demo-eng-sync', 3, 35.5, 58.2, 'Marcus Chen', 'demo-vp-marcus', 'system', 'One: battery reporting drifts after a warm restart. We have a fix, and it ships in RC2 on Thursday.'),

  ('demo-hiring', 1,  0.0, 22.0, 'Priya Shah',  'demo-vp-priya',  'system', 'Let''s go round the room. Overall impressions of Jordan?'),
  ('demo-hiring', 2, 22.0, 45.0, 'Marcus Chen', 'demo-vp-marcus', 'system', 'Strong hire from me. The motor control exercise was the best answer we''ve seen this year.'),
  ('demo-hiring', 3, 45.0, 63.0, 'Me',          NULL,             'microphone', 'Same. Communication was clear, and the questions about our test rig were thoughtful.'),

  ('demo-retro', 1,  0.0, 25.0, 'Marcus Chen', 'demo-vp-marcus', 'system', 'Timeline first. At 02:14 the ingest queue filled up and telemetry stopped for about forty minutes.'),
  ('demo-retro', 2, 25.0, 47.0, 'Priya Shah',  'demo-vp-priya',  'system', 'How did we find out?'),
  ('demo-retro', 3, 47.0, 70.0, 'Marcus Chen', 'demo-vp-marcus', 'system', 'A customer emailed. Our alerting watched the API, not the queue depth, so nothing paged.'),
  ('demo-retro', 4, 70.0, 88.0, 'Me',          NULL,             'microphone', 'So the first action is a queue-depth alert, and the longer fix is the queue rework.');

INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker, voice_profile_id, sequence_id, source, confidence)
SELECT m || '-t' || seq, m, txt,
       strftime('%Y-%m-%dT%H:%M:%S+00:00', (SELECT created_at FROM meetings WHERE id = m), '+' || CAST(s AS INTEGER) || ' seconds'),
       s, e, e - s, who, vp, seq, src, 0.93
FROM seg;
DROP TABLE seg;

-- ---------------------------------------------------------------- chapters & bookmarks

INSERT INTO meeting_chapters (id, meeting_id, position, start_seconds, title, created_at) VALUES
  ('demo-ch-1', 'demo-roadmap', 0,   0.0, 'Offline mode',              strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours')),
  ('demo-ch-2', 'demo-roadmap', 1, 129.5, 'Fleet dashboard redesign',  strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours')),
  ('demo-ch-3', 'demo-roadmap', 2, 226.2, 'Telemetry queue rework',    strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours')),
  ('demo-ch-4', 'demo-roadmap', 3, 327.3, 'Parking self-serve onboarding', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'));

INSERT INTO meeting_bookmarks (id, meeting_id, offset_seconds, label, created_at) VALUES
  ('demo-bm-1', 'demo-roadmap', 300.0, 'Agreed order', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'));

-- ---------------------------------------------------------------- summaries

INSERT INTO summary_processes (meeting_id, status, created_at, updated_at, result, chunk_count, processing_time) VALUES
  ('demo-roadmap', 'completed', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), json_object('markdown',
'**Summary**

The team ranked the Q4 roadmap. Offline mode is the top customer request and will be scoped to 30 minutes of on-device telemetry buffering. The fleet dashboard redesign goes ahead after one more round of testing with Harbor Logistics, and the telemetry queue rework becomes a prerequisite for the dashboard launch. Self-serve onboarding moves to Q1.

**Key Decisions**

- Offline mode buffers 30 minutes of telemetry in Q4; longer buffering is a stretch goal.
- The telemetry queue rework ships before the dashboard launch.
- Alert placement on the dashboard is decided after testing with Harbor Logistics.
- Self-serve onboarding is parked until Q1, first on that list.

**Discussion Highlights**

- Field teams lose the dashboard when Wi-Fi drops, even though the robots keep working.
- Buffering beyond 30 minutes needs a storage review on older hardware.
- Last week''s telemetry outage showed the pipeline can''t take more load without the queue rework.'), 3, 41.2),
  ('demo-design', 'completed', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-5 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-5 hours'), json_object('markdown',
'**Summary**

Elena walked through the new fleet overview: one map for every robot plus a collapsible alert rail. The layout was well received; the map needs clustering to stay fast at a thousand units.

**Key Decisions**

- The alert rail collapses to a status strip when nothing needs attention.
- Map markers cluster at low zoom levels.'), 1, 12.8),
  ('demo-harbor', 'completed', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+16 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+16 hours'), json_object('markdown',
'**Summary**

Harbor Logistics receives 40 units at its Rotterdam site next week. Success means a 15% higher pick rate and a night shift that can resolve alerts without an engineer. Wi-Fi in cold storage is their main concern.

**Key Decisions**

- Weekly check-ins during the pilot.
- A pick-rate report goes out every Monday.'), 1, 15.1),
  ('demo-eng-sync', 'completed', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+10 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day','start of day','+10 hours'), json_object('markdown',
'**Summary**

Firmware 4.2 RC1 is on the test fleet. One regression: battery reporting drifts after a warm restart. The fix ships in RC2 on Thursday.'), 1, 8.4),
  ('demo-hiring', 'completed', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 days','start of day','+16 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 days','start of day','+16 hours'), json_object('markdown',
'**Summary**

The panel agreed on a strong hire for the senior firmware role, citing the motor control exercise and clear communication.'), 1, 6.0),
  ('demo-retro', 'completed', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-6 days','start of day','+11 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-6 days','start of day','+11 hours'), json_object('markdown',
'**Summary**

The ingest queue filled at 02:14 and telemetry stopped for about 40 minutes. Nothing paged because alerting watched the API, not queue depth; a customer reported it.

**Key Decisions**

- Add a queue-depth alert now.
- Plan the queue rework as the long-term fix.'), 1, 10.3);

-- ---------------------------------------------------------------- action items

INSERT INTO action_items (id, meeting_id, text, assignee, due_hint, status, source, created_at, updated_at, completed_at, source_start_secs, source_end_secs, source_quote) VALUES
  ('demo-ai-1', 'demo-roadmap', 'Draft the offline mode spec and share it with Elena', 'Alex Morgan', 'Friday', 'open', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), NULL, 118.0, 129.5, 'I''ll have a first draft by Friday and share it with Elena for the UI states.'),
  ('demo-ai-2', 'demo-roadmap', 'Set up two dashboard test sessions with Harbor Logistics', 'Elena Novak', 'Next week', 'open', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), NULL, 210.7, 226.2, 'I''ll reach out to Sam and set up two sessions.'),
  ('demo-ai-3', 'demo-roadmap', 'Break down the queue rework estimate', 'Marcus Chen', 'Wednesday', 'open', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), NULL, 281.4, 300.0, 'I can have an estimate broken down by Wednesday.'),
  ('demo-ai-4', 'demo-roadmap', 'Send the ranked Q4 list to the wider team', 'Priya Shah', 'Today', 'done', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 hours'), 366.0, 384.5, 'I''ll write up the ranked list and send it to the wider team by end of day.'),
  ('demo-ai-5', 'demo-design', 'Add map clustering to the dashboard spec', 'Elena Novak', NULL, 'open', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-5 hours'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-5 hours'), NULL, 80.6, 96.2, NULL),
  ('demo-ai-6', 'demo-harbor', 'Send the first Monday pick-rate report', 'Alex Morgan', 'Monday', 'open', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'), NULL, 73.8, 95.1, NULL),
  ('demo-ai-7', 'demo-harbor', 'Survey Wi-Fi coverage in the cold storage area', 'Sam Okafor', 'Before go-live', 'open', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'), NULL, 95.1, 112.6, NULL),
  ('demo-ai-8', 'demo-eng-sync', 'Ship firmware 4.2 RC2 with the battery fix', 'Marcus Chen', 'Thursday', 'open', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-1 day'), NULL, 35.5, 58.2, NULL),
  ('demo-ai-9', 'demo-hiring', 'Send the offer to Jordan', 'Priya Shah', NULL, 'done', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-3 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-2 days'), NULL, NULL, NULL),
  ('demo-ai-10','demo-retro', 'Add a queue-depth alert to the on-call rota', 'Marcus Chen', 'This week', 'done', 'summary', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-6 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-4 days'), strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-4 days'), 70.0, 88.0, NULL);

-- ---------------------------------------------------------------- notes & chat

INSERT INTO meeting_notes (id, meeting_id, body, source, created_at) VALUES
  ('demo-note-1', 'demo-roadmap', 'Check whether the 2023 units have enough flash for longer buffering.', 'manual', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-150 minutes'));

INSERT INTO meeting_chat_messages (id, meeting_id, role, content, created_at) VALUES
  ('demo-chat-1', 'demo-roadmap', 'user', 'What did we decide about offline mode?', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-90 minutes')),
  ('demo-chat-2', 'demo-roadmap', 'assistant', 'Offline mode is the top Q4 item. It will buffer 30 minutes of telemetry on the device [01:35], with longer buffering as a stretch goal because older units would need a storage review [01:11]. Alex owns the spec, with a first draft due Friday [01:58].', strftime('%Y-%m-%dT%H:%M:%S+00:00','now','-90 minutes'));
