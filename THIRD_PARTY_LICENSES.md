# Third-party licences

Generated dependency licences will be produced by `cargo about` in CI. This file
records the **upstream projects rszigbee derives data or design from**, whose
obligations do not appear in a dependency graph.

---

## zigbee-herdsman

MIT License. Copyright (c) 2019 Jack Wu, Simen Li, Hedy Wang and Koen Kanters.
<https://github.com/Koenkk/zigbee-herdsman>

Applies to: transcoded ZCL/ZDO definition data, the adapter boundary design, the
interview quirk table, the coordinator backup format handling.

## zigbee-herdsman-converters

MIT License. Copyright (c) 2018 Koen Kanters.
<https://github.com/Koenkk/zigbee-herdsman-converters>

Applies to: every file under `devices/`, and the converter and exposes semantics
that `rszigbee-devices` reproduces.

---

Full MIT text (applies to both of the above):

```
Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in the
Software without restriction, including without limitation the rights to use,
copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the
Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

---

## Zigbee2MQTT

GPL-3.0. <https://github.com/Koenkk/zigbee2mqtt>

**No Zigbee2MQTT code is included in or translated into rszigbee.** It is
referenced as the definition of an external MQTT contract. See the licence section of the README.
