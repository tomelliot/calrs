-- fork_default_week_view
-- Fork: the week timeline is the booking page's default view. Event types
-- left on the old default (month) move to week; a column choice stays.
UPDATE event_types SET default_calendar_view = 'week' WHERE default_calendar_view = 'month';
