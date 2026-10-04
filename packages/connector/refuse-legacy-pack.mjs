/** Development guard: the legacy fixture package must never become a new shipped runtime. */
throw new Error('Legacy JavaScript runtime packaging is retired. Build the native Sidevoice executable with packages/connector-rust/tools/build-native.py; publication is coordinated separately.');
