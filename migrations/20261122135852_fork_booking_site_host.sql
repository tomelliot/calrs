-- fork_booking_site_host
-- Fork (booking sites): the booking host a booking was made on, such as
-- book.coscope.ai. Guest emails use it for their sender, Reply-To and links.
-- NULL = made on the admin host or before sites existed: stock behaviour.
ALTER TABLE bookings ADD COLUMN site_host TEXT;
