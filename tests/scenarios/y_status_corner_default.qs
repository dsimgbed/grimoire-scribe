# rust-only
# Legacy Bar preferences migrate to the status bar: on, at the small height, and never to a floating
# corner. The bar reserves page space in both orientations.
# settings: status_mode=0
# settings: status_on=0
# settings: status_size=2
# settings: status_corner=3
# env: TZ=UTC
wait 500
clock 1700000000
battery 87
expect settings.status_mode is 1
expect settings.status_on is 1
expect settings.status_size is 0
expect settings.status_corner is 0
expect status.shown is 0
tap 965 95
wait 100
tap 1564 2254
wait 700
expect screen is editor
expect status.shown is 1
expect status.reserved is 88
expect status.text has 87%
shot 01-status-bar
rotate 1
wait 1200
expect status.reserved is 88
expect status.shown is 1
shot 02-status-bar-sideways
quit
