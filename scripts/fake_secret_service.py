"""A throwaway, in-memory org.freedesktop.secrets for testing the stream key
storage on a private bus, never the user's keyring:

    dbus-run-session -- bash -c 'python3 -I scripts/fake_secret_service.py & sleep 1;
        cargo test -p vesper-app keyring -- --ignored'

The default collection starts locked: `Unlock` goes through a prompt that
completes at once (`DISMISS=1` dismisses it instead).
"""
import os

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

BUS = "org.freedesktop.secrets"
SERVICE = "org.freedesktop.Secret.Service"
COLLECTION = "org.freedesktop.Secret.Collection"
ITEM = "org.freedesktop.Secret.Item"
PROMPT = "org.freedesktop.Secret.Prompt"
DEFAULT = "/org/freedesktop/secrets/collection/login"


class Prompt(dbus.service.Object):
    def __init__(self, bus, path, action):
        super().__init__(bus, path)
        self.action = action

    @dbus.service.method(PROMPT, in_signature="s")
    def Prompt(self, window_id):
        dismissed = os.environ.get("DISMISS") == "1"
        if not dismissed:
            self.action()
        GLib.idle_add(lambda: self.Completed(dismissed, dbus.String("", variant_level=1)))

    @dbus.service.signal(PROMPT, signature="bv")
    def Completed(self, dismissed, result):
        pass


class Item(dbus.service.Object):
    def __init__(self, service, path, attributes, secret):
        super().__init__(service.bus, path)
        self.service = service
        self.path = path
        self.attributes = attributes
        self.secret = secret

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="ss", out_signature="v")
    def Get(self, interface, name):
        if interface == ITEM and name == "Attributes":
            return dbus.Dictionary(self.attributes, signature="ss")
        raise dbus.exceptions.DBusException(name, name="org.freedesktop.DBus.Error.UnknownProperty")

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        if interface != ITEM:
            return dbus.Dictionary({}, signature="sv")
        return dbus.Dictionary({"Attributes": dbus.Dictionary(self.attributes, signature="ss")}, signature="sv")

    @dbus.service.method(ITEM, out_signature="o")
    def Delete(self):
        del self.service.items[self.path]
        self.remove_from_connection()
        return "/"


class Collection(dbus.service.Object):
    def __init__(self, service, path):
        super().__init__(service.bus, path)
        self.service = service

    @dbus.service.method(COLLECTION, in_signature="a{sv}(oayays)b", out_signature="oo")
    def CreateItem(self, properties, secret, replace):
        if self.service.locked:
            raise dbus.exceptions.DBusException("locked", name="org.freedesktop.Secret.Error.IsLocked")
        attributes = dict(properties.get("org.freedesktop.Secret.Item.Attributes", {}))
        if replace:
            for path, item in list(self.service.items.items()):
                if item.attributes == attributes:
                    item.Delete()
        self.service.count += 1
        path = f"{DEFAULT}/{self.service.count}"
        self.service.items[path] = Item(self.service, path, attributes, bytes(secret[2]))
        return path, "/"


class Service(dbus.service.Object):
    def __init__(self, bus):
        super().__init__(bus, "/org/freedesktop/secrets")
        self.bus = bus
        self.items = {}
        self.count = 0
        self.prompts = 0
        self.locked = True
        self.collection = Collection(self, DEFAULT)

    @dbus.service.method(SERVICE, in_signature="sv", out_signature="vo")
    def OpenSession(self, algorithm, value):
        if algorithm != "plain":
            raise dbus.exceptions.DBusException("unsupported", name="org.freedesktop.DBus.Error.NotSupported")
        return dbus.String("", variant_level=1), "/org/freedesktop/secrets/session/1"

    @dbus.service.method(SERVICE, in_signature="s", out_signature="o")
    def ReadAlias(self, name):
        return DEFAULT if name == "default" else "/"

    @dbus.service.method(SERVICE, in_signature="a{ss}", out_signature="aoao")
    def SearchItems(self, attributes):
        found = [p for p, i in self.items.items()
                 if all(i.attributes.get(k) == v for k, v in attributes.items())]
        return (dbus.Array([] if self.locked else found, signature="o"),
                dbus.Array(found if self.locked else [], signature="o"))

    @dbus.service.method(SERVICE, in_signature="ao", out_signature="aoo")
    def Unlock(self, objects):
        if not self.locked:
            return dbus.Array(objects, signature="o"), "/"
        self.prompts += 1
        path = f"/org/freedesktop/secrets/prompt/{self.prompts}"
        Prompt(self.bus, path, lambda: setattr(self, "locked", False))
        return dbus.Array([], signature="o"), path

    @dbus.service.method(SERVICE, in_signature="aoo", out_signature="a{o(oayays)}")
    def GetSecrets(self, items, session):
        out = {}
        for path in items:
            item = self.items.get(path)
            if item is not None and not self.locked:
                out[path] = (session, dbus.ByteArray(b""), dbus.ByteArray(item.secret), "text/plain")
        return dbus.Dictionary(out, signature="o(oayays)")


dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName(BUS, bus)
Service(bus)
GLib.MainLoop().run()
