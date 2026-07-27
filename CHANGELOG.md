# Changelog

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
