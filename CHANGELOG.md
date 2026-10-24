# Changelog

## [0.16.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.15.0...v0.16.0) (2026-10-24)


### Features

* **agents:** find Claude Code outside PATH (desktop app bundle, native installer) ([#263](https://github.com/sidevoice/sidevoice-connector/issues/263)) ([c807475](https://github.com/sidevoice/sidevoice-connector/commit/c8074758d317a61990b507b62da75d81836049c2))
* **agents:** find Codex inside the ChatGPT desktop app ([#267](https://github.com/sidevoice/sidevoice-connector/issues/267)) ([7df5c77](https://github.com/sidevoice/sidevoice-connector/commit/7df5c7701280afff98931fcdb06f0f51f21acacf))
* **cursor:** editor voice on Windows and Linux through the desktop bridge ([#293](https://github.com/sidevoice/sidevoice-connector/issues/293)) ([c2e286e](https://github.com/sidevoice/sidevoice-connector/commit/c2e286e778b918cfbd68989530889ecf0f138f3b))
* **install:** name the kept models folder from sidevoice-platform ([#252](https://github.com/sidevoice/sidevoice-connector/issues/252)) ([8912d02](https://github.com/sidevoice/sidevoice-connector/commit/8912d021e1993d737f386e1c907246d45050062f))
* **packaging:** windows-x86_64 target, .exe program names, win32-x64 npm package ([#241](https://github.com/sidevoice/sidevoice-connector/issues/241)) ([d433e01](https://github.com/sidevoice/sidevoice-connector/commit/d433e010ac20680e3954aef2888d5e655c004b37))
* **platform:** harness adapters on every OS ([#239](https://github.com/sidevoice/sidevoice-connector/issues/239)) ([1817a05](https://github.com/sidevoice/sidevoice-connector/commit/1817a052f8deb26fe8e3e70103bcdca714fa3f2d))
* **platform:** install pointer record and Windows-safe removal ([#237](https://github.com/sidevoice/sidevoice-connector/issues/237)) ([d12fdf0](https://github.com/sidevoice/sidevoice-connector/commit/d12fdf0e5f76052aeb9d5858d7c82b9d910127cc)), closes [#116](https://github.com/sidevoice/sidevoice-connector/issues/116)
* **platform:** lifecycle concern, one stop request on every OS, and a Windows-clean crate ([#240](https://github.com/sidevoice/sidevoice-connector/issues/240)) ([3d372c6](https://github.com/sidevoice/sidevoice-connector/commit/3d372c65f424dc3503db62d7088bf8da91f390ff))
* **platform:** local channel over interprocess, peer check on every OS ([#222](https://github.com/sidevoice/sidevoice-connector/issues/222)) ([1e0d174](https://github.com/sidevoice/sidevoice-connector/commit/1e0d174ec6de8964b3268d0d35d6a55260b0e204))
* **platform:** local sockets in the user's runtime directory ([#247](https://github.com/sidevoice/sidevoice-connector/issues/247)) ([ba1df18](https://github.com/sidevoice/sidevoice-connector/commit/ba1df186b697ac807ca3e713e7af9ab485b31492))
* **platform:** one data directory per OS and the environment children get ([#238](https://github.com/sidevoice/sidevoice-connector/issues/238)) ([665394e](https://github.com/sidevoice/sidevoice-connector/commit/665394e2efe17fa889c757efb48557da02ac8dc4))
* **platform:** processes behind one interface, on sysinfo and process-wrap ([#223](https://github.com/sidevoice/sidevoice-connector/issues/223)) ([4b57467](https://github.com/sidevoice/sidevoice-connector/commit/4b574675dc232451df3c28039ce60bec3034094e))
* **platform:** service managers behind one trait, Task Scheduler on Windows ([#232](https://github.com/sidevoice/sidevoice-connector/issues/232)) ([0da72a2](https://github.com/sidevoice/sidevoice-connector/commit/0da72a24bf5bec26ca12f619ee3aef65f61960bb))
* **platform:** shared data directory rule; the connector publishes its endpoint ([#248](https://github.com/sidevoice/sidevoice-connector/issues/248)) ([3e94748](https://github.com/sidevoice/sidevoice-connector/commit/3e947489e5d15983defa0ff7da759ece6b194911))
* **platform:** this user on Windows (token SID) ([#219](https://github.com/sidevoice/sidevoice-connector/issues/219)) ([e64b9ab](https://github.com/sidevoice/sidevoice-connector/commit/e64b9ab51fa8e0d1de1ec136522e5562e1414345))
* **release:** ship Windows x64 ([#253](https://github.com/sidevoice/sidevoice-connector/issues/253)) ([01e9372](https://github.com/sidevoice/sidevoice-connector/commit/01e9372e7786e46b238bea1f5f4d10648270764f))


### Bug Fixes

* **agents:** a scan overtaken by another scans again, and install says when it could look at no agent ([#269](https://github.com/sidevoice/sidevoice-connector/issues/269)) ([41ab38b](https://github.com/sidevoice/sidevoice-connector/commit/41ab38b54f13b744c72f501de4f1a6ac9406443f))
* **agents:** connect an agent that was never started (its home is not there yet) ([#268](https://github.com/sidevoice/sidevoice-connector/issues/268)) ([4681c6a](https://github.com/sidevoice/sidevoice-connector/commit/4681c6a7b45ee3d0af06c86eff1ee2c4697623cd))
* **agents:** find the Claude app CLI under Claude-Data; uninstall never skips our entry silently ([#288](https://github.com/sidevoice/sidevoice-connector/issues/288)) ([0369df9](https://github.com/sidevoice/sidevoice-connector/commit/0369df98b8c7c0bf19efaf94d50d0ff8fb5f759b))
* **agents:** find the person's shell in the user database, not $SHELL ([#273](https://github.com/sidevoice/sidevoice-connector/issues/273)) ([35984cc](https://github.com/sidevoice/sidevoice-connector/commit/35984ccd057ec93e02edf6e9b2d6dae5d4d9cf8b))
* **agents:** keep last scan when login PATH capture fails ([#236](https://github.com/sidevoice/sidevoice-connector/issues/236)) ([f6cc045](https://github.com/sidevoice/sidevoice-connector/commit/f6cc045663da1f6170856ef48d220b6a0b772964)), closes [#235](https://github.com/sidevoice/sidevoice-connector/issues/235)
* **agents:** resolve npm-installed agents to their native binary under any package manager ([#294](https://github.com/sidevoice/sidevoice-connector/issues/294)) ([b71a997](https://github.com/sidevoice/sidevoice-connector/commit/b71a9971af958fb4f0fab74242f20db000cc621c)), closes [#284](https://github.com/sidevoice/sidevoice-connector/issues/284)
* **background:** use real package version in Windows resource ([#287](https://github.com/sidevoice/sidevoice-connector/issues/287)) ([ce3d9b3](https://github.com/sidevoice/sidevoice-connector/commit/ce3d9b32cbc7f6174ddde79eb636b802531c06ce))
* **codex:** log why a delivery was queued instead of steered ([#290](https://github.com/sidevoice/sidevoice-connector/issues/290)) ([db92152](https://github.com/sidevoice/sidevoice-connector/commit/db92152d047b36b7be432046d9fded5bda252183))
* **codex:** run the native Codex of an npm install, and start it without a console window ([#277](https://github.com/sidevoice/sidevoice-connector/issues/277)) ([ca0f35e](https://github.com/sidevoice/sidevoice-connector/commit/ca0f35ec88a515284f51485ef15f119d7df66512))
* **install:** a bounded wait for the service, with its diagnosis on timeout ([#264](https://github.com/sidevoice/sidevoice-connector/issues/264)) ([a9183ab](https://github.com/sidevoice/sidevoice-connector/commit/a9183abe6545ab863ff1904df2cf5c13c4809732))
* **platform:** Windows private check refuses inherited grants and unprotected DACLs ([#242](https://github.com/sidevoice/sidevoice-connector/issues/242)) ([b089299](https://github.com/sidevoice/sidevoice-connector/commit/b089299e5adfa9cbaa215cc4122c2de352201e98))
* **test:** keep test socket paths under the macOS limit ([#243](https://github.com/sidevoice/sidevoice-connector/issues/243)) ([91cd68d](https://github.com/sidevoice/sidevoice-connector/commit/91cd68ddbac7edec4cc74c26905a227ed94ae15a))
* **uninstall:** do not refuse the pre-stop check when the running service changes an entry ([#262](https://github.com/sidevoice/sidevoice-connector/issues/262)) ([0104161](https://github.com/sidevoice/sidevoice-connector/commit/01041612079853b1447ce844f64d0ddddff30772))
* **uninstall:** remove the data dir when only our own files are left ([#295](https://github.com/sidevoice/sidevoice-connector/issues/295)) ([40a5f42](https://github.com/sidevoice/sidevoice-connector/commit/40a5f42f6dd7947bff494c8487df3709be5ceda1))
* **windows:** bench steps that PowerShell runs as one line ([#257](https://github.com/sidevoice/sidevoice-connector/issues/257)) ([d908106](https://github.com/sidevoice/sidevoice-connector/commit/d9081060bc0833829c4c9cba24b4114ffa00e3fd))
* **windows:** manual steps that PowerShell runs as one line ([#256](https://github.com/sidevoice/sidevoice-connector/issues/256)) ([0e562ea](https://github.com/sidevoice/sidevoice-connector/commit/0e562eacfbcae858dd5607487660578d129871fc))
* **windows:** run the service jobs through a windowless launcher, sidevoice-background.exe ([#280](https://github.com/sidevoice/sidevoice-connector/issues/280)) ([b1b4ed6](https://github.com/sidevoice/sidevoice-connector/commit/b1b4ed65c527049c095aac612822555c28e6964f))
* **windows:** start the daemon's helper processes without a console window ([#278](https://github.com/sidevoice/sidevoice-connector/issues/278)) ([d143ed2](https://github.com/sidevoice/sidevoice-connector/commit/d143ed2fefeff786e53801fb61c0fd81a455b6e7))
* **windows:** the service task runs its job definition ([#260](https://github.com/sidevoice/sidevoice-connector/issues/260)) ([494c289](https://github.com/sidevoice/sidevoice-connector/commit/494c289a16a0e0c4cfbee7464c8de2f416fbda51))
* **windows:** the test suite passes on Windows ([#254](https://github.com/sidevoice/sidevoice-connector/issues/254)) ([59f8fe1](https://github.com/sidevoice/sidevoice-connector/commit/59f8fe1eb38014b0ca46cd1518eb10eb7f1f5724))


### Performance Improvements

* **codex:** cache parsed versions per process ([#296](https://github.com/sidevoice/sidevoice-connector/issues/296)) ([e20b615](https://github.com/sidevoice/sidevoice-connector/commit/e20b61528f8e6f3487ff766dafc24df87d48399f)), closes [#270](https://github.com/sidevoice/sidevoice-connector/issues/270)

## [0.15.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.14.1...v0.15.0) (2026-10-21)


### ⚠ BREAKING CHANGES

* **delivery:** inject when possible and keep get_messages available ([#176](https://github.com/sidevoice/sidevoice-connector/issues/176))

### Features

* **delivery:** inject when possible and keep get_messages available ([#176](https://github.com/sidevoice/sidevoice-connector/issues/176)) ([88a3b2a](https://github.com/sidevoice/sidevoice-connector/commit/88a3b2a447cd53d6136739c9a094bc466c4526fa))


### Bug Fixes

* **agent:** let only the message's content decide a voice reply ([#212](https://github.com/sidevoice/sidevoice-connector/issues/212)) ([9b8b836](https://github.com/sidevoice/sidevoice-connector/commit/9b8b83648dea29009d787a01285b97a4b85c216a))

## [0.14.1](https://github.com/sidevoice/sidevoice-connector/compare/v0.14.0...v0.14.1) (2026-10-21)


### Bug Fixes

* **core:** ship sidevoice-core 0.5.0 ([#210](https://github.com/sidevoice/sidevoice-connector/issues/210)) ([45db5e1](https://github.com/sidevoice/sidevoice-connector/commit/45db5e17dd3601964c38174893d3f9084c259006))

## [0.14.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.13.1...v0.14.0) (2026-10-21)


### ⚠ BREAKING CHANGES

* **connector:** remove the room concept ([#208](https://github.com/sidevoice/sidevoice-connector/issues/208))

### Bug Fixes

* **claude:** ignore the synthetic placeholder model ([#206](https://github.com/sidevoice/sidevoice-connector/issues/206)) ([5b343ba](https://github.com/sidevoice/sidevoice-connector/commit/5b343ba8bd83fcbc2abcb93863794bfd228296de))
* **cursor:** read the effort from the chat's selected model parameters ([#207](https://github.com/sidevoice/sidevoice-connector/issues/207)) ([30af7ed](https://github.com/sidevoice/sidevoice-connector/commit/30af7eda23ad7df94e0a111ec663724f9ab8e9df))
* **mcp:** catch up on unheard replies implicitly ([#203](https://github.com/sidevoice/sidevoice-connector/issues/203)) ([e1451e3](https://github.com/sidevoice/sidevoice-connector/commit/e1451e3dda7e0a982320795d9952cec0a5eac9c2)), closes [#201](https://github.com/sidevoice/sidevoice-connector/issues/201)
* **test:** wait for a whole pid in the agents gate test ([#205](https://github.com/sidevoice/sidevoice-connector/issues/205)) ([8738368](https://github.com/sidevoice/sidevoice-connector/commit/8738368b0f81892a5e916e564cd308ba2b5b5127)), closes [#118](https://github.com/sidevoice/sidevoice-connector/issues/118)


### Code Refactoring

* **connector:** remove the room concept ([#208](https://github.com/sidevoice/sidevoice-connector/issues/208)) ([ad6ed10](https://github.com/sidevoice/sidevoice-connector/commit/ad6ed105a89c947c30c0ce167491df2c78fefb7f))

## [0.13.1](https://github.com/sidevoice/sidevoice-connector/compare/v0.13.0...v0.13.1) (2026-10-19)


### Bug Fixes

* **core:** ship sidevoice-core 0.4.7 ([#195](https://github.com/sidevoice/sidevoice-connector/issues/195)) ([1de4da4](https://github.com/sidevoice/sidevoice-connector/commit/1de4da4343d3fc99e8a17a5ad866e25403fd9752))

## [0.13.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.12.2...v0.13.0) (2026-10-19)


### Features

* **mcp:** add a read-only voice_diagnose tool ([#191](https://github.com/sidevoice/sidevoice-connector/issues/191)) ([2b5de8a](https://github.com/sidevoice/sidevoice-connector/commit/2b5de8ac26b453abfbbcdce79983c12a7d99646c))


### Bug Fixes

* **claude:** keep the working state until the turn ends ([#190](https://github.com/sidevoice/sidevoice-connector/issues/190)) ([01b7d59](https://github.com/sidevoice/sidevoice-connector/commit/01b7d599d9c482671200f07471249fa36b7b7351))
* **detect:** don't report Cursor's registration as unreadable when it never ran ([#193](https://github.com/sidevoice/sidevoice-connector/issues/193)) ([81e2530](https://github.com/sidevoice/sidevoice-connector/commit/81e2530f849c49bf2991bcda291068dbbf033bab))
* **transcript:** read a transcript to its end at once on attach ([#188](https://github.com/sidevoice/sidevoice-connector/issues/188)) ([5145e84](https://github.com/sidevoice/sidevoice-connector/commit/5145e8431445e0d800349ef974e9cbbd577c2821))

## [0.12.2](https://github.com/sidevoice/sidevoice-connector/compare/v0.12.1...v0.12.2) (2026-10-18)


### Bug Fixes

* **cursor:** treat a bridge status error as a failed send and log its reason ([#184](https://github.com/sidevoice/sidevoice-connector/issues/184)) ([29897d2](https://github.com/sidevoice/sidevoice-connector/commit/29897d272a9373f5b5a092ad11f678eff448baea))

## [0.12.1](https://github.com/sidevoice/sidevoice-connector/compare/v0.12.0...v0.12.1) (2026-10-18)


### Bug Fixes

* **cursor:** log unconfirmed bridge replies safely ([#181](https://github.com/sidevoice/sidevoice-connector/issues/181)) ([0affe13](https://github.com/sidevoice/sidevoice-connector/commit/0affe13d696d35b01067cfa9efdcfa1e7bb9839b)), closes [#180](https://github.com/sidevoice/sidevoice-connector/issues/180) [#173](https://github.com/sidevoice/sidevoice-connector/issues/173)

## [0.12.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.11.0...v0.12.0) (2026-10-18)

v0.11.0 remains a draft release. This release includes its pending changes as well as the changes below.

### Features

* **codex:** steer a running turn when the installed Codex supports it ([#168](https://github.com/sidevoice/sidevoice-connector/issues/168)) ([a29f60b](https://github.com/sidevoice/sidevoice-connector/commit/a29f60b7030c0ac293dcd983dfa4db89894fdbe8))
* **cursor:** report recorded chat model and effort ([#136](https://github.com/sidevoice/sidevoice-connector/issues/136)) ([ab2f678](https://github.com/sidevoice/sidevoice-connector/commit/ab2f67884b4516815172d026dfde64ffdec18b9d)), closes [#133](https://github.com/sidevoice/sidevoice-connector/issues/133)


### Bug Fixes

* **daemon:** replay explicit agent leaves after core recovery ([#167](https://github.com/sidevoice/sidevoice-connector/issues/167)) ([243d024](https://github.com/sidevoice/sidevoice-connector/commit/243d0244e44d436eb762efdca9474666939c50a7))
* **cursor:** trace and complete Desktop Bridge push delivery ([#173](https://github.com/sidevoice/sidevoice-connector/issues/173)) ([82bae14](https://github.com/sidevoice/sidevoice-connector/commit/82bae14e921c8ca0e70251ab5297aa6c58791156)), closes [#172](https://github.com/sidevoice/sidevoice-connector/issues/172)

### CI

* Move macOS jobs to macos-15 ([#170](https://github.com/sidevoice/sidevoice-connector/issues/170)) ([3b536fd](https://github.com/sidevoice/sidevoice-connector/commit/3b536fd8ff396ba8cc0b26703c0e81f3cc6afdcc))

## [0.11.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.10.4...v0.11.0) (2026-10-18)


### Features

* **codex:** steer a running turn when the installed Codex supports it ([#168](https://github.com/sidevoice/sidevoice-connector/issues/168)) ([a29f60b](https://github.com/sidevoice/sidevoice-connector/commit/a29f60b7030c0ac293dcd983dfa4db89894fdbe8))


### Bug Fixes

* **daemon:** replay explicit agent leaves after core recovery ([#167](https://github.com/sidevoice/sidevoice-connector/issues/167)) ([243d024](https://github.com/sidevoice/sidevoice-connector/commit/243d0244e44d436eb762efdca9474666939c50a7))

## [0.10.4](https://github.com/sidevoice/sidevoice-connector/compare/v0.10.3...v0.10.4) (2026-10-18)


### Bug Fixes

* **pairing:** show LAN address and request device codes ([#162](https://github.com/sidevoice/sidevoice-connector/issues/162)) ([cd8ec48](https://github.com/sidevoice/sidevoice-connector/commit/cd8ec4838bd2129c17bf1c6e6054ccaadbca7268))

## [0.10.3](https://github.com/sidevoice/sidevoice-connector/compare/v0.10.2...v0.10.3) (2026-10-17)


### Bug Fixes

* **service:** listen on the network with the compatible core ([#158](https://github.com/sidevoice/sidevoice-connector/issues/158)) ([4a21ff9](https://github.com/sidevoice/sidevoice-connector/commit/4a21ff9aed9f6816d55f35d2ae84eab23aec6fb5))

## [0.10.2](https://github.com/sidevoice/sidevoice-connector/compare/v0.10.1...v0.10.2) (2026-10-16)


### Bug Fixes

* **detect:** find agents whose PATH is set in the shell's rc file ([#149](https://github.com/sidevoice/sidevoice-connector/issues/149)) ([c507326](https://github.com/sidevoice/sidevoice-connector/commit/c5073264fc039be7686571b7ad96d0b7c13747e4))
* **voice:** tell the agent plainly what wasn't heard, and to say it again if it matters ([#152](https://github.com/sidevoice/sidevoice-connector/issues/152)) ([c41b257](https://github.com/sidevoice/sidevoice-connector/commit/c41b257506f39426c74e0686ec8711c7d20ac369)), closes [#153](https://github.com/sidevoice/sidevoice-connector/issues/153)

## [0.10.1](https://github.com/sidevoice/sidevoice-connector/compare/v0.10.0...v0.10.1) (2026-10-16)


### Bug Fixes

* **cli:** tell people to run the connector with npx ([#145](https://github.com/sidevoice/sidevoice-connector/issues/145)) ([6a5162e](https://github.com/sidevoice/sidevoice-connector/commit/6a5162ebcb10ff16f24ea6d01579c1f430b5de6e))

## [0.10.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.9.0...v0.10.0) (2026-10-15)


### Features

* **cursor:** expose Desktop Bridge setup and readiness ([#130](https://github.com/sidevoice/sidevoice-connector/issues/130)) ([f610f60](https://github.com/sidevoice/sidevoice-connector/commit/f610f6057c69aa7f9d05c6dfb0f525a1b32dcace)), closes [#120](https://github.com/sidevoice/sidevoice-connector/issues/120)
* **detect:** find agents via login PATH and passive markers ([#139](https://github.com/sidevoice/sidevoice-connector/issues/139)) ([b369577](https://github.com/sidevoice/sidevoice-connector/commit/b3695776ff29d5bd8bc3b6af853c35c40507b4f8)), closes [#134](https://github.com/sidevoice/sidevoice-connector/issues/134)
* **turns:** carry conversation epochs through connector delivery ([#126](https://github.com/sidevoice/sidevoice-connector/issues/126)) ([4bf9a90](https://github.com/sidevoice/sidevoice-connector/commit/4bf9a903b9307dd60c4ae4cfc06667b800809719))


### Bug Fixes

* **agents:** an agent is installed by its program, never by its configuration directory ([#128](https://github.com/sidevoice/sidevoice-connector/issues/128)) ([0b1d6a8](https://github.com/sidevoice/sidevoice-connector/commit/0b1d6a83ddd435f169d9c10554c6bb745cba16ce))
* **cursor:** expose bridge delivery diagnostics ([#135](https://github.com/sidevoice/sidevoice-connector/issues/135)) ([1aeb09c](https://github.com/sidevoice/sidevoice-connector/commit/1aeb09c00d985ae1ffdf5a3345eca678721d650b)), closes [#131](https://github.com/sidevoice/sidevoice-connector/issues/131)
* **uninstall:** confine deletion to owned roots ([#127](https://github.com/sidevoice/sidevoice-connector/issues/127)) ([ed2d0f9](https://github.com/sidevoice/sidevoice-connector/commit/ed2d0f959008aa8498ffb0a24a67d2a5b4653a5a))
* **working:** wake Cursor monitor when bridge identifies chat ([#137](https://github.com/sidevoice/sidevoice-connector/issues/137)) ([f69a451](https://github.com/sidevoice/sidevoice-connector/commit/f69a45105e78ffec4a5a6f208af60c6b736dd0d6)), closes [#132](https://github.com/sidevoice/sidevoice-connector/issues/132)

## [0.9.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.8.1...v0.9.0) (2026-10-14)


### Features

* **connector:** check that a connected agent's MCP entry starts and answers ([#108](https://github.com/sidevoice/sidevoice-connector/issues/108)) ([6dcef8d](https://github.com/sidevoice/sidevoice-connector/commit/6dcef8d34656b6a806580409d3d548184284d10e))
* **connector:** Cursor's editor chats are reached only through its Desktop Bridge ([#115](https://github.com/sidevoice/sidevoice-connector/issues/115)) ([3a3d54b](https://github.com/sidevoice/sidevoice-connector/commit/3a3d54b17fad1bb427ea13139e4c36c4cd9be869))
* **connector:** report outdated agent entries and act on them ([#105](https://github.com/sidevoice/sidevoice-connector/issues/105)) ([e251b5d](https://github.com/sidevoice/sidevoice-connector/commit/e251b5d159520cce914f2242f1d1b241e8e052bb))
* **connector:** say per agent what the connector can do and the manual steps ([#107](https://github.com/sidevoice/sidevoice-connector/issues/107)) ([b45f7a3](https://github.com/sidevoice/sidevoice-connector/commit/b45f7a36552fad9bf801389038541f7e283b2385))
* **connector:** service login on|off|status, start at login without starting or stopping ([#106](https://github.com/sidevoice/sidevoice-connector/issues/106)) ([237ce13](https://github.com/sidevoice/sidevoice-connector/commit/237ce13899a3c6cd60ba63ab8c5ba05e8724d7e9))
* **connector:** the core's default port is 10829 ([#119](https://github.com/sidevoice/sidevoice-connector/issues/119)) ([6a56cdb](https://github.com/sidevoice/sidevoice-connector/commit/6a56cdb6f2e39610f9dcaf08029a57478ccdf4d9))


### Bug Fixes

* **connector:** a Codex conversation reports its effort, the model's default when none was chosen ([#123](https://github.com/sidevoice/sidevoice-connector/issues/123)) ([e28f24d](https://github.com/sidevoice/sidevoice-connector/commit/e28f24dc4983dad6aac294dafa56b022399cb7d2))
* **connector:** every wire reader ignores unknown fields and enum values ([#117](https://github.com/sidevoice/sidevoice-connector/issues/117)) ([5ee9348](https://github.com/sidevoice/sidevoice-connector/commit/5ee93480e28fbf2bf92751afc00e135ad6acaeb7))
* **connector:** pass CLAUDE_CONFIG_DIR to Claude Code only when the person's environment sets it ([#110](https://github.com/sidevoice/sidevoice-connector/issues/110)) ([da54743](https://github.com/sidevoice/sidevoice-connector/commit/da5474303093aa3f7d13fed1ddfe2e1fcf1991a7))
* **connector:** report each conversation's reasoning effort from its transcript ([#99](https://github.com/sidevoice/sidevoice-connector/issues/99)) ([a554912](https://github.com/sidevoice/sidevoice-connector/commit/a5549120962c63e78f3315be283a9d98a66cf32e))
* **install:** installing registers Sidevoice with no agent; the person connects each one ([#121](https://github.com/sidevoice/sidevoice-connector/issues/121)) ([101a826](https://github.com/sidevoice/sidevoice-connector/commit/101a82691cd8e508cfff4153648bededb7b47912))

## [0.8.1](https://github.com/sidevoice/sidevoice-connector/compare/v0.8.0...v0.8.1) (2026-10-10)


### Bug Fixes

* **connector:** read each tool call's _meta where rmcp puts it, so Codex Desktop joins voice ([#97](https://github.com/sidevoice/sidevoice-connector/issues/97)) ([e90ca99](https://github.com/sidevoice/sidevoice-connector/commit/e90ca99d681bb93c3271e3921a553f004d38cafe))

## [0.8.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.7.1...v0.8.0) (2026-10-10)


### ⚠ BREAKING CHANGES

* **connector:** take the room-only core archive (rust-native-v2), no self-test ([#91](https://github.com/sidevoice/sidevoice-connector/issues/91))

### Features

* **connector:** take the room-only core archive (rust-native-v2), no self-test ([#91](https://github.com/sidevoice/sidevoice-connector/issues/91)) ([ff0644c](https://github.com/sidevoice/sidevoice-connector/commit/ff0644cb50e9c964e91c303bec7e619130c12fde))
* **connector:** tell the agent what the user did not hear ([#90](https://github.com/sidevoice/sidevoice-connector/issues/90)) ([14f868d](https://github.com/sidevoice/sidevoice-connector/commit/14f868d5bbb2d23dda1a5b540adbdcafb16f0bd6))


### Bug Fixes

* agents reach and leave this installation, systemd enables for the next login, install repairs a damaged core ([#94](https://github.com/sidevoice/sidevoice-connector/issues/94)) ([1f33488](https://github.com/sidevoice/sidevoice-connector/commit/1f33488428ae8247c136959cfa147ec030dc8a59))
* **bench:** the page names its API relative to itself ([#92](https://github.com/sidevoice/sidevoice-connector/issues/92)) ([0b925d4](https://github.com/sidevoice/sidevoice-connector/commit/0b925d48e1c72049e12049d488d61b03f9d85b33))

## [0.7.1](https://github.com/sidevoice/sidevoice-connector/compare/v0.7.0...v0.7.1) (2026-10-09)


### Bug Fixes

* **mcp:** answer current Claude Code's server/discover with valid lists (rmcp 3.5.1) ([#87](https://github.com/sidevoice/sidevoice-connector/issues/87)) ([134f993](https://github.com/sidevoice/sidevoice-connector/commit/134f993e6dfc87c62678c1fce7d67f672e78b204))

## [0.7.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.6.0...v0.7.0) (2026-10-06)


### Features

* add host agent management to connector ([#33](https://github.com/sidevoice/sidevoice-connector/issues/33)) ([0a4c62b](https://github.com/sidevoice/sidevoice-connector/commit/0a4c62bd1a28e094d2986cc7c76043c3b0b858eb))
* add isolated Rust connector proof ([#40](https://github.com/sidevoice/sidevoice-connector/issues/40)) ([ea27002](https://github.com/sidevoice/sidevoice-connector/commit/ea27002d10498b4bf29df0f0dab7e527dcaa8b91))
* add proof-private Rust host agents ([#43](https://github.com/sidevoice/sidevoice-connector/issues/43)) ([02a2f06](https://github.com/sidevoice/sidevoice-connector/commit/02a2f06fedcfacb74ae5a4b139c4fa8881c2a3c6))
* add Rust conversation adapters for Claude and Cursor ([#52](https://github.com/sidevoice/sidevoice-connector/issues/52)) ([c0468ba](https://github.com/sidevoice/sidevoice-connector/commit/c0468ba8a705661dc30c5ebc2b743eec7e370fa5))
* **bench:** a test bench in Rust to talk to real agent sessions without the app ([#84](https://github.com/sidevoice/sidevoice-connector/issues/84)) ([d778ee3](https://github.com/sidevoice/sidevoice-connector/commit/d778ee36e43ad562242b0f5089814ae6bcd48a2d))
* **browser-audio:** the page's models come from sidevoice-core's catalogue, and its resolver in TypeScript ([1757c24](https://github.com/sidevoice/sidevoice-connector/commit/1757c24011ddb0bd80fd48641d9d39329078c8e0)), closes [#124](https://github.com/sidevoice/sidevoice-connector/issues/124)
* **connector:** Cursor CLI as a harness — the room hears it, and cannot talk to it ([b5eb9e4](https://github.com/sidevoice/sidevoice-connector/commit/b5eb9e4c9fbaef1541c3978c11a7033ff54dbdf5))
* **connector:** from a checkout, install the core wheel put beside the sources ([7df3bbc](https://github.com/sidevoice/sidevoice-connector/commit/7df3bbceac24e427039b3028d20feca1b58fbb25))
* **connector:** install and supervise this machine's core, and link to it over loopback ([23c23f7](https://github.com/sidevoice/sidevoice-connector/commit/23c23f7870085f9a5533f89b5a1a51ada36e46c5))
* **connector:** install and uninstall in Rust, with automatic rollback and agent registration ([#78](https://github.com/sidevoice/sidevoice-connector/issues/78)) ([bd1e64c](https://github.com/sidevoice/sidevoice-connector/commit/bd1e64c79147d0fc3aea6d0db520836815fcfbcc))
* **connector:** pair and pair-device in Rust, with the device code's QR ([#73](https://github.com/sidevoice/sidevoice-connector/issues/73)) ([85504f2](https://github.com/sidevoice/sidevoice-connector/commit/85504f25793d42c187b2b761e7407297d792d720))
* **connector:** pair this machine with a room when its core asks ([e99915d](https://github.com/sidevoice/sidevoice-connector/commit/e99915da7a3c01de9d4c373f13f389162baea51e))
* **connector:** service management in Rust (launchd, systemd, on demand) ([#71](https://github.com/sidevoice/sidevoice-connector/issues/71)) ([55568c8](https://github.com/sidevoice/sidevoice-connector/commit/55568c8fc25539a7062fe83858e310d7a22cd405))
* **connector:** ship source-built Rust Connector and Core pair ([435fd31](https://github.com/sidevoice/sidevoice-connector/commit/435fd315e657a4f1372fc524f773387797195f38))
* **connector:** Sidevoice at login as two native jobs; immutable releases with atomic switch and rollback (onboarding R1-b) ([#25](https://github.com/sidevoice/sidevoice-connector/issues/25)) ([249ca5e](https://github.com/sidevoice/sidevoice-connector/commit/249ca5e0ffb052a0832236981561f1c53afb088b))
* **connector:** the core travels inside the package, staged and self-tested at install ([#74](https://github.com/sidevoice/sidevoice-connector/issues/74)) ([72902e4](https://github.com/sidevoice/sidevoice-connector/commit/72902e4564c6389bee0a53168433ce1db7b1fc95))
* **cursor:** experimental delivery — tmux for persist chats, an MCP App view for the editor ([10466d7](https://github.com/sidevoice/sidevoice-connector/commit/10466d799225676e8e214553c431ba5d6f618b84))
* **cursor:** say why an editor chat has no card, and log what Cursor declared ([d30df94](https://github.com/sidevoice/sidevoice-connector/commit/d30df94cf4aaf8b384df6ab47e36e4b04ca7c4d1))
* **cursor:** several editor chats of one window join the room at once ([ae8d793](https://github.com/sidevoice/sidevoice-connector/commit/ae8d7935c26438d3dd46a7f6c5858a82f3270d98))
* **cursor:** two ticks for editor chats, and the room shows Cursor and its route ([cabd3ca](https://github.com/sidevoice/sidevoice-connector/commit/cabd3ca4944a8423cd57d7bf730524d68e99ccc2))
* **cursor:** voice into an editor chat by its id, through Cursor's Desktop Bridge ([2aab66a](https://github.com/sidevoice/sidevoice-connector/commit/2aab66a0726c8ca771bf91d6d918ddbb8a0dad71))
* implement Sigstore-verified Node SEA core installation ([#29](https://github.com/sidevoice/sidevoice-connector/issues/29)) ([c509c95](https://github.com/sidevoice/sidevoice-connector/commit/c509c95d2d54e933f3c1041f5ebbca4b68c9019d))
* **install:** install, start and health-check this machine's core before registering ([7d26d4f](https://github.com/sidevoice/sidevoice-connector/commit/7d26d4f512832e063ab889e797c4dd0d4e398d39))
* **mcp:** with no room paired, a conversation joins this machine's core ([4f383b3](https://github.com/sidevoice/sidevoice-connector/commit/4f383b3570bb4f7db84a28cabf302ddeb4be6034))
* **npm:** publish the connector as sidevoice with one package per platform ([#75](https://github.com/sidevoice/sidevoice-connector/issues/75)) ([f42cc7d](https://github.com/sidevoice/sidevoice-connector/commit/f42cc7d7c765a7f0cf5ac437ae8b2ddd56076b9f))
* **room, connector:** device pairing — relay it, pass the token, issue codes from the node ([8ec1657](https://github.com/sidevoice/sidevoice-connector/commit/8ec16576d0b19e5d8e7322715059b6e9175f9d21))
* **rust:** add private macOS service lifecycle proof ([#47](https://github.com/sidevoice/sidevoice-connector/issues/47)) ([f2589c7](https://github.com/sidevoice/sidevoice-connector/commit/f2589c7cbe8f5037e6f1d85e177f2d0cb4db196c))
* the microphone over WebRTC, signalled through the room, with the relay as the fallback ([9707e5d](https://github.com/sidevoice/sidevoice-connector/commit/9707e5d20e6f37330b66ab4103b6b5f29a0a0829))
* the room without the core — rendezvous and relay between browsers and machines ([d7da067](https://github.com/sidevoice/sidevoice-connector/commit/d7da067a085f27bd571814faea6bf69fcbce85d2))


### Bug Fixes

* **connector:** give voice_pair_device time for a core that is still starting ([#82](https://github.com/sidevoice/sidevoice-connector/issues/82)) ([1e39517](https://github.com/sidevoice/sidevoice-connector/commit/1e3951711e3ec5cc5e661d545c9d17f64dc15f5a))
* **connector:** pin core 0.2.1, which runs on glibc 2.28 ([#85](https://github.com/sidevoice/sidevoice-connector/issues/85)) ([2096803](https://github.com/sidevoice/sidevoice-connector/commit/2096803eb9c126eb2a316c9269131fed026eee05))
* **core:** create the core's environment with uv's own Python, never the machine's ([#20](https://github.com/sidevoice/sidevoice-connector/issues/20)) ([6d5e27e](https://github.com/sidevoice/sidevoice-connector/commit/6d5e27e92c314e0251f7092b8fac9601c5a0e627))
* **cursor:** a second chat of a window no longer takes the first one's voice ([782d2c1](https://github.com/sidevoice/sidevoice-connector/commit/782d2c1eb8891a7f0724179bce7750bacd3580b1))
* **cursor:** an editor chat can speak first; log every method Cursor calls ([c8609dd](https://github.com/sidevoice/sidevoice-connector/commit/c8609dd14d4ff4b77952a347bf184fa44dd37af2))
* **cursor:** identify the editor chat by its transcript, and log every bridge step ([c31657d](https://github.com/sidevoice/sidevoice-connector/commit/c31657d1d3ced5f5b4aea18a73d05ce104d2eeda))
* **cursor:** what the quick review found in speaking first ([e139d66](https://github.com/sidevoice/sidevoice-connector/commit/e139d66e2448195093ae861112bcfef92ace4bf7))
* **cursor:** what the quick review found in the Desktop Bridge route ([0178bce](https://github.com/sidevoice/sidevoice-connector/commit/0178bce666d2d140f3662a5c014617f96d67d781))
* **cursor:** what the review found — kept mcp.json, rejoin, rewrites, no retries ([ab6e407](https://github.com/sidevoice/sidevoice-connector/commit/ab6e407c086d0f371cfa29a1bec68a9ee673e790))
* **cursor:** what the review found in several chats per window ([787bfce](https://github.com/sidevoice/sidevoice-connector/commit/787bfce9a52b5ea168011e43c4f00f20d58f915a))
* **cursor:** what the review found in the experimental routes ([3ee97e0](https://github.com/sidevoice/sidevoice-connector/commit/3ee97e0c176db1138667a604944a618a897cae89))
* HTTPS-only pairing in the web, honest relay docs, explicit cluster allowlist in the connector ([#134](https://github.com/sidevoice/sidevoice-connector/issues/134)) ([9df6a53](https://github.com/sidevoice/sidevoice-connector/commit/9df6a535f8ec6b621d40e8a88df978873f0290a8))
* install dependencies before desktop pin verification ([#31](https://github.com/sidevoice/sidevoice-connector/issues/31)) ([86a0ab1](https://github.com/sidevoice/sidevoice-connector/commit/86a0ab1a5363af303b25f77fc7254aef5db1fea3))
* **install:** with no room paired, say that voice reaches this machine's own app, not that a room's code is needed ([fdb7d4a](https://github.com/sidevoice/sidevoice-connector/commit/fdb7d4ac1e8ba33c6c0528455bba0e0a6474a6cb))
* pin actual core attestation builder identity ([#30](https://github.com/sidevoice/sidevoice-connector/issues/30)) ([30c1757](https://github.com/sidevoice/sidevoice-connector/commit/30c175715d95d45d73b847a73328cb8dcf622b1a))
* protect queued speech outbox ([#44](https://github.com/sidevoice/sidevoice-connector/issues/44)) ([7c86f28](https://github.com/sidevoice/sidevoice-connector/commit/7c86f28766b7a844f23f0b369a0222101072c1c4))
* what the adversarial review confirmed ([0f70fbb](https://github.com/sidevoice/sidevoice-connector/commit/0f70fbb199b1590e584944950eaed084abd4faa8))
