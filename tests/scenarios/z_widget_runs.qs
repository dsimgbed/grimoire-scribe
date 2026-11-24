# settings: status_mode=1
# rust-only
# env: GRIMOIRE_SIM_CLOCK=script
# copy: tests/fixtures/widgetruns .
# Widget modules that run for long (issue #208), with the packages of tests/fixtures/widgetruns (written by
# rust/tests/widgetfix: `make widget-fixtures`). App time is the script's (GRIMOIRE_SIM_CLOCK=script): each
# `wait` moves it on by exactly its milliseconds, so what comes due in a stretch is the same on any machine.
wait 3600
tap 965 95
wait 100
tap 1564 2254
wait 300
expect screen is editor
# Loop's drawing pass never ends. The call is ended when its instructions are used up, and the frame says that
# the widget stopped. Its manifest names no size: the frame is the default one, 220x160.
tap panel widgets
wait 300
expect editor.widgets.open is 1
tap panel widgets.test.runs.loop
wait 400
expect editor.widgets is 1 test.runs.loop
expect editor.widget.0.rect is 1456 28 220 160
expect editor.widget.0.error is The widget stopped: all fuel consumed by WebAssembly
expect editor.widget.0.hits is 0
shot 01-loop
# (the frame is still a frame: it closes)
tap panel widget.0.close
wait 300
expect editor.widgets is 0
quit
