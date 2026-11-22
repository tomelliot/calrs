# calrs fork: booking sites

This fork of [olivierlambert/calrs](https://github.com/olivierlambert/calrs) serves several booking hosts from one instance. Each host has its own skin, its own meeting types and its own email sender. The meeting types of each site are chosen in the event type form, by domain. calrs is AGPL-3.0; this fork is published under the same licence, and each booking site links to its source.

## What the fork changes

| Change | Where |
| --- | --- |
| `sites.toml`: host → skin, title, user, sender | `src/web/sites.rs` (new) |
| "Booking sites" in the event type form: one checkbox per domain, stored in `event_type_sites`, copied on duplicate, shown as badges on the event types list | migration `*_fork_event_type_sites.sql`, `src/web/event_types.rs`, `templates/event_type_form.html`, `src/web/dashboard.rs`, `templates/dashboard_event_types.html` |
| Request host → site; on a site host only public booking paths answer; types not ticked for the site and every admin path are 404 | `sites::site_middleware`, layer in `src/web/mod.rs` |
| Skins served at `/skin/<name>/<file>`, linked after the page CSS; logo, favicon, title and footer | `src/web/sites.rs`, `templates/base.html`, `templates/profile.html`, `templates/partials/public_logo.html`, `src/web/branding.rs` |
| `/` on a site host lists the types ticked for the site | `root_redirect` in `src/web/dashboard.rs`, filter in `user_profile` (`src/web/public_pages.rs`) |
| Booking stores its site (`bookings.site_host`); guest emails use the site's From, Reply-To, links and ICS organizer | migration `*_fork_booking_site_host.sql`, `record_booking` in `src/web/booking.rs`, `Sender::Host.uid` in `src/email/transport.rs`, `site_rewrite` in `src/email/guest.rs` and `render.rs` |
| `site-source-link`, `etf-booking-sites`, `etf-booking-sites-hint` strings | `i18n/*/main.ftl` |
| Slot page: week timeline (open time = union of each start plus the duration, hover snaps to a start, keyboard access), sidebar month calendar, header with Today, arrows, view select and language menu, footer row with the timezone. Week is the default view (new types; migration moves `month` types to `week`). Below 900px the timeline shows one day | `src/web/timeline.rs` (new), migration `*_fork_default_week_view.sql`, `templates/partials/timeline_style.html` and `timeline_script.html` (new), hooks in `templates/slots.html`, `src/web/event_types.rs`, `templates/event_type_form.html`, `src/commands/event_type.rs` |
| Booking dialog: a start opens its booking form in a `<dialog>`; `GET …/book?partial=1` renders the form alone (`partials/book_dialog.html`) with its CSRF field, and it posts to the usual handler. Reschedule mode and embeds keep the page link. Slot pages get the booking form CSP (captcha) | `templates/partials/book_form.html` (form moved out of `book.html`), `timeline::book_template` in `src/web/public_pages.rs`, `timeline_middleware` and `csp_middleware` in `src/web/mod.rs` |
| Language menu: a `calrs_lang` cookie stands in for `Accept-Language` | `timeline_middleware` |
| Site footer as a partial, so the slot page places it in its footer row | `templates/partials/site_footer.html`, `templates/base.html` |
| `tl-*` and `etf-view-month-short` strings | `i18n/*/main.ftl` |
| Propose times (`/dashboard/propose`): pick a meeting under a booking site, click open times in a week, copy them as an email snippet whose times open the booking form at that time. Nothing is stored | `src/web/propose.rs` (new), routes and `mod propose` in `src/web/mod.rs`, `templates/propose_meeting.html`, `templates/propose_times.html`, `templates/partials/propose_head.html` (new), sidebar link in `templates/dashboard_base.html`, Phosphor `paper-plane-tilt` |
| Slot computation shared by the REST API and propose times (`offered_slots`), the titled busy events of troubleshoot and propose times (`titled_calendar_events`), `blocking_calendar_names`, `slot_interval` | `src/web/availability.rs`, `api_slots` in `src/web/api.rs` |
| Week grid shared by the slot page and propose times: axis and zone, day headers, hatched closed time, open regions, snapped preview, selected blocks, keyboard, header bar, legend pills, the timezone row, and page layers (busy blocks, proposals, guest hours and night) | `templates/partials/week_grid_style.html`, `week_grid_script.html`, `week_grid_tz.html` (new); `timeline_style.html` and `timeline_script.html` keep the slot page layout, mini calendar and booking dialog; `templates/slots.html` uses the timezone row |
| `nav-propose-times`, `propose-*` strings | `i18n/*/main.ftl` |
| Tests | `src/web/sites.rs`, `src/web/tests/sites.rs`, `src/web/timeline.rs`, `src/web/tests/timeline.rs`, `src/web/tests/propose.rs` |

Every hook in an upstream file carries a `Fork:` comment: `git grep -n "Fork:"` lists them.

Without `sites.toml` the fork behaves exactly like upstream.

## Branches and rebasing

- `upstream`: mirrors `olivierlambert/calrs` `main`.
- `main`: the fork, rebased onto the latest upstream release tag.

```sh
git fetch upstream --tags
git rebase --onto vX.Y.Z vPREVIOUS main
cargo test
scripts/check-migrations.sh vX.Y.Z
git tag vX.Y.Z-site.1 && git push origin main --tags --force-with-lease
```

Tags build `ghcr.io/tomelliot/calrs:<tag>` (`.github/workflows/publish.yml`). Fork tags are prereleases, so `latest` does not move: deploy by tag.

## Configuration

`<data_dir>/sites.toml` and `<data_dir>/skins/<name>/`, from `site/` in this repo. A skin needs `skin.css`; optional `logo.svg|png|webp`, `logo-dark.*`, `favicon.svg|png|ico`, and `*.woff2` fonts named from the CSS by relative URL. Skin CSS overrides the theme variables (`--bg`, `--surface`, `--text`, `--text-secondary`, `--text-muted`, `--border`, `--accent`, `--accent-hover`, `--radius`, …) in `:root` (light) and `html.dark` (dark). calrs puts white text on `--accent`, so keep it dark enough for that.

An invalid `sites.toml` stops calrs at startup with a message naming the problem.

**Moving a site to another host.** Rename its `[sites."<host>"]` key and list the old host in `aliases = ["<old host>"]`. Requests on the old host get a 308 redirect to the same path on the new one, so links in emails already sent keep working. At startup, the event type choices and bookings stored under the old host move to the new one. Keep the old host in DNS and the tunnel as long as old links matter.

**Guest emails of a site booking** come from its `from_email` (default: the instance sender), show `from_name` or else the host's name, reply to `reply_to`, link to the site, end with the site's title, and name `reply_to` as the organizer in every calendar file, the host's calendar copy included.

## Deploying on imp

Hosts: `calrs.tomelliot.net` (admin, `CALRS_BASE_URL`, behind Cloudflare Access), `calendar.tomelliot.net`, `calendar.coscope.ai`.

1. **Data directory.** `mkdir -p /var/lib/calrs/skins`, copy `site/sites.toml` and `site/skins/*`, then `chown -R` it to the image's `calrs` user (`docker run --rm --entrypoint id ghcr.io/tomelliot/calrs:<tag> calrs`).
2. **Secrets.** In Bitwarden Secrets Manager, project `imp`: `CALRS_SECRET_KEY` (base64 of 32 random bytes), `CALRS_FASTMAIL_SMTP_PASSWORD` and `CALRS_FASTMAIL_CALDAV_PASSWORD` (Fastmail app passwords, SMTP only and CalDAV only). Put the Fastmail login in `/opt/calrs/.env` as `CALRS_SMTP_USERNAME=…`.
3. **Start.** Copy `deploy/compose.yml` to `/opt/calrs/` and `deploy/calrs.service` to `/etc/systemd/system/`, put `CALRS_TAG=<tag>` and `CALRS_SMTP_USERNAME=<login>` in `/opt/calrs/.env`, then `systemctl daemon-reload && systemctl enable --now calrs`. The unit fetches the secrets from bws at every start, so calrs comes back after a reboot. To deploy a release, change `CALRS_TAG` and `systemctl restart calrs`.
4. **Tunnel.** In Zero Trust → Networks → Tunnels → imp → Public hostnames, add the three hosts, each to `http://localhost:3000`. This creates their DNS records. calrs itself answers 404 for admin paths on the booking hosts.
5. **Access.** Zero Trust → Access → Applications: one self-hosted application for `calrs.tomelliot.net`, policy = your email.
6. **calrs.** `docker exec -it calrs calrs user create --email … --name "Tom Elliot" --admin` (username `tom`), then on `calrs.tomelliot.net`: enable MFA, turn registration off, add the Fastmail CalDAV source (app password, CalDAV only), pick the write-back calendar, upload the avatar, and create the event types, ticking their domains under "Booking sites".
7. **Check.** On each booking host: book a slot, see it in Fastmail, and check that the guest email comes from that domain with DKIM `pass`.

Backups: `calrs.db` (with `sqlite3 .backup`), off-site. The key is `CALRS_SECRET_KEY` in bws, not a `secret.key` file: without it the stored CalDAV password cannot be read.
