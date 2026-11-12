# World Clock

Shows the time in one or more cities. With several cities it shows one at a time and rotates between them.
The tooltip shows the weekday and the UTC offset, for example `Tokyo · Thu · UTC+9`.

Settings: `cities`, a list of `Label|Time zone` entries such as `Tokyo|Asia/Tokyo` (IANA zone names),
and `rotate`, the seconds each city stays on screen (default 6).

Data: the computer's clock and the time zone database built into Perchbar. No network access.
