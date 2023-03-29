[![crates.io](https://img.shields.io/crates/v/cln-zapper.svg)](https://crates.io/crates/cln-zapper)
[![CI](https://github.com/thesimplekid/cln-zapper-rs/actions/workflows/lint.yml/badge.svg)](https://github.com/thesimplekid/cln-zapper-rs/actions/workflows/lint.yml)
[![CI](https://github.com/thesimplekid/cln-zapper-rs/actions/workflows/test.yml/badge.svg)](https://github.com/thesimplekid/cln-zapper-rs/actions/workflows/test.yml)

# Core Lightning plugin for sending zap events

You can add the plugin by copying it to CLN's plugin directory or by adding the following line to your config file:

```
plugin=/path/to/cln-zapper
```

This plugin only handles the zap note. You will also need an lnurl server to create invoice like https://github.com/elsirion/clnurl. I would recommend setting that up first.

This guide can be followed for more details on how to set up: https://github.com/TheFockinFury/Non-Custodial-Nostr-Zaps-CLN


## Options
`cln-zapper` exposes the following config options that can be included in CLN's config file or as command line flags:
* `clnzapper_nostr_nsec`: The nostr private key used to sign zapper notes
* `clnzapper_nostr_relay`: The default nostr relay to publish to

## License

Code is under the [BSD 3-Clause License](LICENSE-BSD-3)

## Contribution

All contributions welcome.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, shall be licensed as above, without any additional terms or conditions.

## Contact

I can be contacted for comments or questions on nostr at _@thesimplekid.com (npub1qjgcmlpkeyl8mdkvp4s0xls4ytcux6my606tgfx9xttut907h0zs76lgjw) or via email tsk@thesimplekid.com.


## Credit
https://github.com/elsirion/clnurl

https://github.com/jb55/cln-nostr-zapper
