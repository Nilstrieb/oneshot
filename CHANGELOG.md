# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](http://semver.org/spec/v2.0.0.html).

### Categories each change fall into

* **Added**: for new features.
* **Changed**: for changes in existing functionality.
* **Deprecated**: for soon-to-be removed features.
* **Removed**: for now removed features.
* **Fixed**: for any bug fixes.
* **Security**: in case of vulnerabilities.


## [Unreleased]
### Changed
- Upgrade to Rust edition 2021. Also increases the MSRV to Rust 1.60.
- Add null-pointer optimization to `Sender`, `Receiver` and `SendError`.
  This reduces the call stack size of Sender::send and it makes
  `Option<Sender>` and `Option<Receiver>` pointer sized.

### Fixed
- Fix undefined behavior due to multiple mutable references to the same channel instance
- Fix unsafe ownership code, telling dropck that the Sender/Receiver/SendError owns the
  (potentially) contained message, and might drop it. This prevents some unsound code
  from compiling. Even if users of this crate are very unlikely to have ever written
  such code, this is strictly breaking, as it stops some (invalid) code from compiling.


## [0.1.3] - 2021-11-23
### Fixed
- Keep the *last* `Waker` in `Future::poll`, not the *first* one. Stops breaking the contract
  on how futures should work.


## [0.1.2] - 2020-08-11
### Fixed
- Fix unreachable code panic that happened if the `Receiver` of an empty but open channel was
  polled and then dropped.


## [0.1.1] - 2020-05-10
Initial implementation. Supports basically all the (for now) intended functionality.
Sender is as lock-free as I think it can get and the receiver can both do thread blocking
and be awaited asynchronously. The receiver also has a wait-free `try_recv` method.

The crate has two features. They are activated by default, but the user can opt out of async
support as well as usage of libstd (making the crate `no_std` but still requiring liballoc)


## [0.1.0] - 2019-05-30
Name reserved on crate.io by someone other than the author of this crate.
