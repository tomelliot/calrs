-- fork_event_type_sites
-- Fork (booking sites): the booking sites an event type is offered on, by
-- host as configured in sites.toml (book.coscope.ai). Chosen in the event
-- type form. A type with no row is on no booking site.
CREATE TABLE event_type_sites (
    event_type_id TEXT NOT NULL REFERENCES event_types(id) ON DELETE CASCADE,
    site_host TEXT NOT NULL,
    PRIMARY KEY (event_type_id, site_host)
);
