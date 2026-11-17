---
"loro-crdt": patch
---

Drop the `im` dependency (and with it `sized-chunks` and `bitmaps`, all MPL-2.0). `ImVersionVector` now stores its entries in a small persistent hash trie of its own, with the same O(1) clones and structurally shared updates, so the package contains only permissively licensed code. Behaviour and encodings are unchanged.
