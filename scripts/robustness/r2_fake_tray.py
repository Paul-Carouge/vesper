"""A StatusNotifierWatcher for the lab's private bus, so Vesper has a tray.

usage: r2_fake_tray.py <t1> [t2 ...]

Each argument is a time in seconds after the first item registered: the item
gets a left click (`Activate`, which shows or hides the window) then. Run it
inside the lab's D-Bus session, before Vesper.
"""
import os
import sys
import time

# Never take the tray name on the real session bus.
if os.listdir("/run/user") or "/run/user" in os.environ.get("DBUS_SESSION_BUS_ADDRESS", ""):
    sys.exit("r2_fake_tray.py: not in the lab (/run/user visible), refusing to run")

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

NAME = "org.kde.StatusNotifierWatcher"
IFACE = "org.kde.StatusNotifierWatcher"
PROPS = {
    "IsStatusNotifierHostRegistered": True,
    "ProtocolVersion": dbus.Int32(0),
    "RegisteredStatusNotifierItems": dbus.Array([], signature="s"),
}


class Watcher(dbus.service.Object):
    def __init__(self, bus, times):
        super().__init__(bus, "/StatusNotifierWatcher")
        self.bus, self.times, self.done = bus, times, False

    @dbus.service.method(IFACE, in_signature="s")
    def RegisterStatusNotifierItem(self, service):
        print(f"item registered: {service}", flush=True)
        if self.done:
            return
        self.done = True
        for t in self.times:
            GLib.timeout_add(int(t * 1000), self.click, str(service))

    def click(self, service):
        # `service` is a bus name, or a path on the sender.
        try:
            name, path = (service, "/StatusNotifierItem")
            if service.startswith("/"):
                name, path = (self.bus.get_name_owner(NAME), service)
            obj = self.bus.get_object(name, path)
            obj.Activate(0, 0, dbus_interface="org.kde.StatusNotifierItem")
            print(f"{time.monotonic():.1f} clicked {service}", flush=True)
        except Exception as e:
            print(f"click failed: {e}", flush=True)
        return False

    @dbus.service.signal(IFACE, signature="s")
    def StatusNotifierHostRegistered(self, s=""):
        pass

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="ss", out_signature="v")
    def Get(self, iface, name):
        return PROPS[name]

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="s", out_signature="a{sv}")
    def GetAll(self, iface):
        return PROPS


dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName(NAME, bus)
w = Watcher(bus, [float(a) for a in sys.argv[1:]])
print("watcher up", flush=True)
GLib.MainLoop().run()
