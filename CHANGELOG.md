# Changelog

## [0.1.3](https://github.com/sean-reid/jamstream/compare/v0.1.2...v0.1.3) (2026-07-28)


### Bug Fixes

* **ci:** freshen armed pull requests by commit count, not BEHIND ([#136](https://github.com/sean-reid/jamstream/issues/136)) ([fd74f8a](https://github.com/sean-reid/jamstream/commit/fd74f8af1ce4cef194cbddb223bfae4885354b62))
* **cli:** a loopback session must not claim a LAN address ([#135](https://github.com/sean-reid/jamstream/issues/135)) ([eb307f8](https://github.com/sean-reid/jamstream/commit/eb307f8bb773f991a7920b26ac28f422ecae947d))
* **client:** settings becomes a drawer that fits the window ([#131](https://github.com/sean-reid/jamstream/issues/131)) ([b8b735a](https://github.com/sean-reid/jamstream/commit/b8b735a2a410ff3c1b1d517ce29593ca179c6c17))
* **client:** stop the test binaries reading the real keychain ([#119](https://github.com/sean-reid/jamstream/issues/119)) ([f3cd9b0](https://github.com/sean-reid/jamstream/commit/f3cd9b0c6abc350092ec3d9a3a5bb83e91cf1c32))
* **cli:** let a local session bind an address from the environment ([#134](https://github.com/sean-reid/jamstream/issues/134)) ([040efc0](https://github.com/sean-reid/jamstream/commit/040efc02be8b955e6777bc697b9c36178f66b11f))
* **cloud:** a region fact that is absent is neither zero nor fatal ([#120](https://github.com/sean-reid/jamstream/issues/120)) ([c907238](https://github.com/sean-reid/jamstream/commit/c907238debedf29ebd8a87c51dba30022173e13e))
* **cloud:** grant the security group permissions AWS launches need ([#118](https://github.com/sean-reid/jamstream/issues/118)) ([9f59e6c](https://github.com/sean-reid/jamstream/commit/9f59e6c81cad8573cf4f539fa5352914c0e8f013)), closes [#114](https://github.com/sean-reid/jamstream/issues/114)
* **cloud:** hold the closed port instead of hoping nobody takes it ([#109](https://github.com/sean-reid/jamstream/issues/109)) ([aa44ace](https://github.com/sean-reid/jamstream/commit/aa44acecbd4a1c1314f851e97d00881a3e8f1bba))
* **cloud:** make the local dead man's switch fail closed ([#130](https://github.com/sean-reid/jamstream/issues/130)) ([4a71166](https://github.com/sean-reid/jamstream/commit/4a7116645661e211ab8f654523e5b14747d8fcf4))
* **cloud:** offer loopback alongside the LAN address in local invites ([#132](https://github.com/sean-reid/jamstream/issues/132)) ([afba727](https://github.com/sean-reid/jamstream/commit/afba7273d20fee6e3e59317b0f496749dc05b0ef))
* **server:** let jamstreamd be told which address to listen on ([#128](https://github.com/sean-reid/jamstream/issues/128)) ([da6b92a](https://github.com/sean-reid/jamstream/commit/da6b92a32c0aba4e5eee856cc7731fd6d8ff6b08))

## [0.1.2](https://github.com/sean-reid/jamstream/compare/v0.1.1...v0.1.2) (2026-07-28)


### Features

* app icon ([#22](https://github.com/sean-reid/jamstream/issues/22)) ([8481602](https://github.com/sean-reid/jamstream/commit/8481602a7c6c7b56e8dc69ab2799cc0a0a169e69))
* audio device layer ([3285c7f](https://github.com/sean-reid/jamstream/commit/3285c7f366edce60cd46d1d97af170818fd3ec5e))
* audio engine ([bf89d11](https://github.com/sean-reid/jamstream/commit/bf89d111ec2c27105cbc861aee139f1a234a5aef))
* avatars in the app, with mtu-safe chunks ([#28](https://github.com/sean-reid/jamstream/issues/28)) ([a85ebbf](https://github.com/sean-reid/jamstream/commit/a85ebbf53ba07c44cd9f832f0e56b4381d39d010))
* aws provider ([1d1c98e](https://github.com/sean-reid/jamstream/commit/1d1c98ef25dd0d43e675069beb72e539e39b52dc))
* broadcast card renderer ([#7](https://github.com/sean-reid/jamstream/issues/7)) ([d825485](https://github.com/sean-reid/jamstream/commit/d825485dc8939ad0eb0147339bdd0a01d470cb71))
* broadcast mix controls and host audition ([7223bef](https://github.com/sean-reid/jamstream/commit/7223bef6381d913890c3d420e1ce5a9b775214c7))
* capture-pacing drift compensation ([b7b9d98](https://github.com/sean-reid/jamstream/commit/b7b9d985e24df7a8afd90d035b1a55915e2d79d7))
* **client:** pick an avatar from a file and fit it automatically ([#97](https://github.com/sean-reid/jamstream/issues/97)) ([b628f03](https://github.com/sean-reid/jamstream/commit/b628f03b972157efa4f4f2d14d43257f36b7f4eb))
* cloud error detail, gcp pagination and pricing, runtime ami resolution ([6406aec](https://github.com/sean-reid/jamstream/commit/6406aecdac8757ee303b0f4e50b581284323956e))
* cloud provisioning core ([7196f2d](https://github.com/sean-reid/jamstream/commit/7196f2d25faf8b7a452c35fb3410725b7165418e))
* desktop client interface ([4cf59df](https://github.com/sean-reid/jamstream/commit/4cf59df3889047fd79ff9e7fcda656de9427961d))
* deterministic simulation substrate ([5c0e839](https://github.com/sean-reid/jamstream/commit/5c0e839492983813b76020fee8c66d9cc639b1f3))
* digitalocean provider ([3fac846](https://github.com/sean-reid/jamstream/commit/3fac846987fc151b12835cdcdb98cf0f8d84719c))
* gcp provider ([879748d](https://github.com/sean-reid/jamstream/commit/879748d3ffc4d3b0cdc1419610ec73a92109943e))
* hard session cap for local mode ([#2](https://github.com/sean-reid/jamstream/issues/2)) ([f518f8a](https://github.com/sean-reid/jamstream/commit/f518f8a6447c8dfd7ccb58a2bc589d978b6554e7))
* jamstream cli ([e630e13](https://github.com/sean-reid/jamstream/commit/e630e13290f2579d0ab891ceb8bc47727ff4442c))
* jamstreamd session server ([dcac5e1](https://github.com/sean-reid/jamstream/commit/dcac5e1682a24a22d01fe96fbfaadcd3722408cc))
* live client runtime ([adc38e6](https://github.com/sean-reid/jamstream/commit/adc38e63e7060136ba0f0e3b1c6ef9125adc283c))
* local is the default way to host ([34c10fa](https://github.com/sean-reid/jamstream/commit/34c10fa4180af5b00a2bc9130d9ff16a18a8484f))
* local session mode ([ca25bb3](https://github.com/sean-reid/jamstream/commit/ca25bb3bd73e77c2b4f13220a01ff53478e7d3a4))
* native gcp service-account auth ([2c15528](https://github.com/sean-reid/jamstream/commit/2c1552884ea7e4afb3e4c68aa9b641a0063c33b4))
* object storage and the recording cost model ([#38](https://github.com/sean-reid/jamstream/issues/38)) ([7eef25e](https://github.com/sean-reid/jamstream/commit/7eef25e577956c84cd8d2fcf5b994b2e42d4c143))
* one line install and a downloads page ([#6](https://github.com/sean-reid/jamstream/issues/6)) ([4005456](https://github.com/sean-reid/jamstream/commit/40054565f51367ebfe6714cc15ef749e0dd8672c))
* package manager manifests ([#27](https://github.com/sean-reid/jamstream/issues/27)) ([4d7e469](https://github.com/sean-reid/jamstream/commit/4d7e469e01360f3af910026752dbaef2549998c5))
* polite http layer for provider implementations ([3f551d8](https://github.com/sean-reid/jamstream/commit/3f551d806fa5ca8a7719d7a06a504d07a304068c))
* real hosting from the app ([#5](https://github.com/sean-reid/jamstream/issues/5)) ([cb4ad44](https://github.com/sean-reid/jamstream/commit/cb4ad4440d969dbb172bce872905b4189be124a0))
* session cores ([4ab7310](https://github.com/sean-reid/jamstream/commit/4ab7310845e0b78943cada27b5361aa66823de7b))
* session wire protocol ([2578a7f](https://github.com/sean-reid/jamstream/commit/2578a7feb419ba0fd276204c0a7f8fe5e3a7e790))
* signed release artifacts ([#3](https://github.com/sean-reid/jamstream/issues/3)) ([2331cd6](https://github.com/sean-reid/jamstream/commit/2331cd6530bdb3af9b6cc2d0dd144ff55a46816d))
* stats reporting, handshake retry, and protocol polish ([e1d4bb0](https://github.com/sean-reid/jamstream/commit/e1d4bb0a6efe7f174545f68950eb97e555f0cf82))
* stream mix panel and host audition ([#20](https://github.com/sean-reid/jamstream/issues/20)) ([ccb4830](https://github.com/sean-reid/jamstream/commit/ccb48302eb06ac1c0b8ee62bc454a3f75e887e4c))
* windows exclusive mode audio ([#37](https://github.com/sean-reid/jamstream/issues/37)) ([7683213](https://github.com/sean-reid/jamstream/commit/7683213c87f3a68fc0736758b2f0e0a321d0384c))


### Bug Fixes

* accept a base64 or raw notarization key ([#19](https://github.com/sean-reid/jamstream/issues/19)) ([690a409](https://github.com/sean-reid/jamstream/commit/690a4093fe177f72ca508133e082d65033add3ec))
* aws cost preview understated the bill by half ([f3e1d1e](https://github.com/sean-reid/jamstream/commit/f3e1d1ecfd04184afdc27ade1ef13724b24d2b94))
* beta releases resolve as latest ([#17](https://github.com/sean-reid/jamstream/issues/17)) ([4c200b1](https://github.com/sean-reid/jamstream/commit/4c200b1e7d8a6c43f1458918516c57f8c6dddcc3))
* **ci:** build release artifacts from the tag, not from main ([#74](https://github.com/sean-reid/jamstream/issues/74)) ([4853579](https://github.com/sean-reid/jamstream/commit/4853579c14fbc351b37bc624f51c62f82d1f84c3))
* **ci:** let release-please update Cargo.lock with the version bump ([#105](https://github.com/sean-reid/jamstream/issues/105)) ([e5d2daf](https://github.com/sean-reid/jamstream/commit/e5d2daff734cce0551425384942425c84fe246a1))
* **ci:** refresh Cargo.lock on the release branch after the bump ([#111](https://github.com/sean-reid/jamstream/issues/111)) ([973259d](https://github.com/sean-reid/jamstream/commit/973259dcf65410d5f313e70aff383e43cc12b670))
* **ci:** revert the rust release type, it breaks release-please ([#112](https://github.com/sean-reid/jamstream/issues/112)) ([a568159](https://github.com/sean-reid/jamstream/commit/a568159ddc431bd63d249f2cc95d662d0e41e946))
* **ci:** unbreak main, then remove the blind spot that broke it ([#99](https://github.com/sean-reid/jamstream/issues/99)) ([8c9c140](https://github.com/sean-reid/jamstream/commit/8c9c140568d1eee8dbabec814f4c065a12f6f960))
* **client:** free the seat when an invite is revoked ([#110](https://github.com/sean-reid/jamstream/issues/110)) ([b18401b](https://github.com/sean-reid/jamstream/commit/b18401b733fc12b239f9ddd02957f902bbf15cbb))
* **client:** keep the fader clear of the member name at narrow widths ([#85](https://github.com/sean-reid/jamstream/issues/85)) ([d70d359](https://github.com/sean-reid/jamstream/commit/d70d359c35fecd8dea015a4784b7a6cfab0ff6ec))
* **cli:** keep private state out of /tmp and off symlinks ([#80](https://github.com/sean-reid/jamstream/issues/80)) ([006d693](https://github.com/sean-reid/jamstream/commit/006d693cae7be356dbee00da915e40032da62528))
* **cli:** keep session keys out of Debug and off disk once a session ends ([#88](https://github.com/sean-reid/jamstream/issues/88)) ([cd80fa0](https://github.com/sean-reid/jamstream/commit/cd80fa0b067427c46eeefc19fab05decdd369b42))
* **cli:** validate the artifact pair and keep it out of the boot script ([#87](https://github.com/sean-reid/jamstream/issues/87)) ([c798309](https://github.com/sean-reid/jamstream/commit/c7983096080a9acc8d31947d4895dd6d118e8267))
* **cloud:** check process identity before killing a pid on unix ([#77](https://github.com/sean-reid/jamstream/issues/77)) ([087dfd8](https://github.com/sean-reid/jamstream/commit/087dfd8f730e6833b14dc76b5cb0535b026e86ae))
* **cloud:** honour --max-hours on GCP and stop leaving stopped VMs behind ([#82](https://github.com/sean-reid/jamstream/issues/82)) ([323444f](https://github.com/sean-reid/jamstream/commit/323444f9b99b2bfc37c5a622d83c793318ce23d3))
* **cloud:** make the local registry survive a second process and a bad byte ([#89](https://github.com/sean-reid/jamstream/issues/89)) ([a2bc3aa](https://github.com/sean-reid/jamstream/commit/a2bc3aa9d991bf96d8321aa3a1ab04979bdfa114))
* jitter buffer diverges permanently under slow-clock drift ([a432db1](https://github.com/sean-reid/jamstream/commit/a432db1235fa41b8d583e1d31aa65fa9bc7dacf0))
* jitter buffer heals unreconcilable playout positions ([#26](https://github.com/sean-reid/jamstream/issues/26)) ([f64cbd3](https://github.com/sean-reid/jamstream/commit/f64cbd3ec990f3aa36b2f06e238e862970d91bb1))
* notarize the dmg, not just the app inside it ([#23](https://github.com/sean-reid/jamstream/issues/23)) ([32cdc52](https://github.com/sean-reid/jamstream/commit/32cdc52a2736da52b5a982754562424c32b244a0))
* **protocol:** build the reject bench key the way the server does ([#108](https://github.com/sean-reid/jamstream/issues/108)) ([49dd9e0](https://github.com/sean-reid/jamstream/commit/49dd9e0a3ff5f34374dc4e372a23d82052e7acd8))
* **protocol:** hold frames outside the receive window instead of dropping them ([#83](https://github.com/sean-reid/jamstream/issues/83)) ([edd874a](https://github.com/sean-reid/jamstream/commit/edd874a954ab81c0e3474acc10ce54aaa3ebc0c9))
* **session:** keep the handshake state when a response fails to verify ([#95](https://github.com/sean-reid/jamstream/issues/95)) ([df8932a](https://github.com/sean-reid/jamstream/commit/df8932a133d0570c7cc1b30316ebb012017b5d07))
* **session:** reap a connection whose control link has given up ([#102](https://github.com/sean-reid/jamstream/issues/102)) ([fe9ca30](https://github.com/sean-reid/jamstream/commit/fe9ca30c98a0fb8f0994a1704e95835afc625346))
* windows local sessions shut down gracefully and safely ([#33](https://github.com/sean-reid/jamstream/issues/33)) ([d5645e2](https://github.com/sean-reid/jamstream/commit/d5645e25ea71138f74c4299ef6ea774fb58ffe35))


### Performance Improvements

* **session:** encode the broadcast frame once, not once per listener ([#78](https://github.com/sean-reid/jamstream/issues/78)) ([61f4168](https://github.com/sean-reid/jamstream/commit/61f41684879c345e51ed21918a183804cd054989))

## [0.1.1-beta](https://github.com/sean-reid/jamstream/compare/v0.1.0...v0.1.1-beta) (2026-07-27)


### Features

* audio device layer ([3285c7f](https://github.com/sean-reid/jamstream/commit/3285c7f366edce60cd46d1d97af170818fd3ec5e))
* audio engine ([bf89d11](https://github.com/sean-reid/jamstream/commit/bf89d111ec2c27105cbc861aee139f1a234a5aef))
* aws provider ([1d1c98e](https://github.com/sean-reid/jamstream/commit/1d1c98ef25dd0d43e675069beb72e539e39b52dc))
* broadcast card renderer ([#7](https://github.com/sean-reid/jamstream/issues/7)) ([d825485](https://github.com/sean-reid/jamstream/commit/d825485dc8939ad0eb0147339bdd0a01d470cb71))
* broadcast mix controls and host audition ([7223bef](https://github.com/sean-reid/jamstream/commit/7223bef6381d913890c3d420e1ce5a9b775214c7))
* capture-pacing drift compensation ([b7b9d98](https://github.com/sean-reid/jamstream/commit/b7b9d985e24df7a8afd90d035b1a55915e2d79d7))
* cloud error detail, gcp pagination and pricing, runtime ami resolution ([6406aec](https://github.com/sean-reid/jamstream/commit/6406aecdac8757ee303b0f4e50b581284323956e))
* cloud provisioning core ([7196f2d](https://github.com/sean-reid/jamstream/commit/7196f2d25faf8b7a452c35fb3410725b7165418e))
* desktop client interface ([4cf59df](https://github.com/sean-reid/jamstream/commit/4cf59df3889047fd79ff9e7fcda656de9427961d))
* deterministic simulation substrate ([5c0e839](https://github.com/sean-reid/jamstream/commit/5c0e839492983813b76020fee8c66d9cc639b1f3))
* digitalocean provider ([3fac846](https://github.com/sean-reid/jamstream/commit/3fac846987fc151b12835cdcdb98cf0f8d84719c))
* gcp provider ([879748d](https://github.com/sean-reid/jamstream/commit/879748d3ffc4d3b0cdc1419610ec73a92109943e))
* hard session cap for local mode ([#2](https://github.com/sean-reid/jamstream/issues/2)) ([f518f8a](https://github.com/sean-reid/jamstream/commit/f518f8a6447c8dfd7ccb58a2bc589d978b6554e7))
* jamstream cli ([e630e13](https://github.com/sean-reid/jamstream/commit/e630e13290f2579d0ab891ceb8bc47727ff4442c))
* jamstreamd session server ([dcac5e1](https://github.com/sean-reid/jamstream/commit/dcac5e1682a24a22d01fe96fbfaadcd3722408cc))
* live client runtime ([adc38e6](https://github.com/sean-reid/jamstream/commit/adc38e63e7060136ba0f0e3b1c6ef9125adc283c))
* local is the default way to host ([34c10fa](https://github.com/sean-reid/jamstream/commit/34c10fa4180af5b00a2bc9130d9ff16a18a8484f))
* local session mode ([ca25bb3](https://github.com/sean-reid/jamstream/commit/ca25bb3bd73e77c2b4f13220a01ff53478e7d3a4))
* native gcp service-account auth ([2c15528](https://github.com/sean-reid/jamstream/commit/2c1552884ea7e4afb3e4c68aa9b641a0063c33b4))
* one line install and a downloads page ([#6](https://github.com/sean-reid/jamstream/issues/6)) ([4005456](https://github.com/sean-reid/jamstream/commit/40054565f51367ebfe6714cc15ef749e0dd8672c))
* polite http layer for provider implementations ([3f551d8](https://github.com/sean-reid/jamstream/commit/3f551d806fa5ca8a7719d7a06a504d07a304068c))
* real hosting from the app ([#5](https://github.com/sean-reid/jamstream/issues/5)) ([cb4ad44](https://github.com/sean-reid/jamstream/commit/cb4ad4440d969dbb172bce872905b4189be124a0))
* session cores ([4ab7310](https://github.com/sean-reid/jamstream/commit/4ab7310845e0b78943cada27b5361aa66823de7b))
* session wire protocol ([2578a7f](https://github.com/sean-reid/jamstream/commit/2578a7feb419ba0fd276204c0a7f8fe5e3a7e790))
* signed release artifacts ([#3](https://github.com/sean-reid/jamstream/issues/3)) ([2331cd6](https://github.com/sean-reid/jamstream/commit/2331cd6530bdb3af9b6cc2d0dd144ff55a46816d))
* stats reporting, handshake retry, and protocol polish ([e1d4bb0](https://github.com/sean-reid/jamstream/commit/e1d4bb0a6efe7f174545f68950eb97e555f0cf82))


### Bug Fixes

* aws cost preview understated the bill by half ([f3e1d1e](https://github.com/sean-reid/jamstream/commit/f3e1d1ecfd04184afdc27ade1ef13724b24d2b94))
* jitter buffer diverges permanently under slow-clock drift ([a432db1](https://github.com/sean-reid/jamstream/commit/a432db1235fa41b8d583e1d31aa65fa9bc7dacf0))
