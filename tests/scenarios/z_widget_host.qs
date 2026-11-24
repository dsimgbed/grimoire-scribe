# settings: status_mode=1
# rust-only
# copy: tests/fixtures/widgethost .
# What the widget host lets an installed module do (issue #208), with the packages of tests/fixtures/widgethost
# (written by rust/tests/widgetfix: `make widget-fixtures`). Each is a module written for one rule of the host,
# which it reaches and passes.
wait 3600
tap 965 95
wait 100
tap 1564 2254
wait 300
expect screen is editor
# Wide draws a black button from far above and left of its body to far below and right of it, under a hit region
# as large. The frame is 400x300 at 1276,28: its body is 372x214 at 1290,100, and only the body is drawn on.
tap panel widgets
wait 300
expect editor.widgets.open is 1
tap panel widgets.test.host.wide
wait 400
expect editor.widgets is 1 test.host.wide
expect editor.widget.0.rect is 1276 28 400 300
expect editor.widget.0.error is
shot 01-wide
# (every pixel of the body is black, so the module did draw)
expect display.dark.1290.100.372.214 is 79608
# The title bar between the name and the minimize button, the page above, left of, right of and below the frame
# (where the popup was, too) are as they were: nothing dark.
expect display.dark.1430.36.110.44 is 0
expect display.dark.1276.2.400.24 is 0
expect display.dark.1000.30.270.220 is 0
expect display.dark.1680.30.20.180 is 0
expect display.dark.1280.334.390.50 is 0
# A tap in the body reaches the module's region; the status line says which region the last tap hit.
expect editor.widget.0.hits is 2
tap panel widget.0.hit.7
wait 300
expect editor.widget.0.status is 7
# A tap in the frame's margin, beside the body, is in none of the module's regions.
tap 1281 323
wait 300
expect editor.widget.0.status is /
# The whole screen drawn again (a new page) draws the frame again, and still only its body.
tap panel next
wait 600
expect editor.page is 2
expect display.dark.1290.100.372.214 is 79608
expect display.dark.1430.36.110.44 is 0
expect display.dark.1000.30.270.220 is 0
expect display.dark.1280.334.390.50 is 0
expect display.dark.200.400.900.40 is 0
shot 02-wide-new-page
tap panel widget.0.close
wait 300
expect editor.widgets is 0
# Plain is the same module in a package whose manifest declares no capability. Its frame opens at the same place;
# nothing the module draws is shown, it has no hit region, and a tap leaves no state for the status line to say.
tap panel widgets
wait 300
tap panel widgets.test.host.plain
wait 400
expect editor.widgets is 1 test.host.plain
expect editor.widget.0.rect is 1276 28 400 300
expect editor.widget.0.error is
expect editor.widget.0.hits is 0
expect display.dark.1290.100.372.214 is 0
shot 03-plain
tap 1476 207
wait 300
expect editor.widget.0.status is
expect editor.widget.0.hits is 0
tap panel widget.0.close
wait 300
expect editor.widgets is 0
# Many draws 513 rectangles in a pass, one more than a pass may hold: the pass is stopped as one that used up its
# instructions is, none of it is drawn, and the frame says that the widget stopped.
tap panel widgets
wait 300
tap panel widgets.test.host.many
wait 400
expect editor.widgets is 1 test.host.many
expect editor.widget.0.error is The widget stopped: all fuel consumed by WebAssembly
expect editor.widget.0.hits is 0
expect editor.widget.0.text is
shot 04-many
# (the frame is still a frame: it closes)
tap panel widget.0.close
wait 300
expect editor.widgets is 0
quit
