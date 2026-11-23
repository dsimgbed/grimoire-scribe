# rust-only
# The notebook status bar is a reserved strip; one finger pulled down from it opens Quick settings,
# whose sliders move the front light and whose buttons set Rotation lock, Auto brightness and Settings.
# env: TZ=UTC GRIMOIRE_SIM_LIGHT_SENSOR=1
# settings: status_on=1
# settings: status_size=1
wait 500
clock 1700000000
battery 87
tap 965 95
wait 100
tap 1564 2254
wait 700
expect screen is editor
expect status.reserved is 104
expect status.shown is 1
expect editor.quick is 0
shot 01-status-bar
# One finger, starting on the status bar, pulled down: the panel.
touch down 700 50
touch move 700 110
touch move 700 220
touch up 700 220
wait 300
expect editor.quick is 1
expect settings.status_on is 1
expect editor.quick.bright.rect is 30 306 1800 116
expect editor.quick.warm.rect is 30 438 1800 116
shot 02-quick-settings
# Brightness follows the tap, warmth too.
tap 900 364
wait 200
expect light.intensity not 12
expect editor.quick.brightness not 12
tap 1500 496
wait 200
expect editor.quick.warmth not 8
expect light.warmth not 8
# The buttons at the top: rotation lock, auto brightness.
tap 945 211
wait 200
expect settings.rotation_lock is 1
expect editor.quick is 1
tap 1565 211
wait 200
expect light.auto is 1
expect editor.quick is 1
# A tap outside closes it; another pull opens it again on Settings.
tap 900 1300
wait 200
expect editor.quick is 0
touch down 700 50
touch move 700 120
touch move 700 230
touch up 700 230
wait 300
expect editor.quick is 1
tap 324 211
wait 700
expect library.sheet is settings
expect editor.quick is 0
# Settings > Sleep & Clock > Status bar: its Size row, and no preview or corner row.
tap library settings.cat.2
wait 100
tap library clock-settings
wait 300
expect library.sheet is display
shot 03-status-bar-settings
tap library display.size
wait 300
expect settings.status_size is 2
tap library display.done
wait 700
expect editor.modal is none
expect status.reserved is 124
shot 04-status-bar-large
quit
