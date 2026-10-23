package dev.waltr.app;

final class RecordAudioPermissionLaunch {
  private RecordAudioPermissionLaunch() {}

  static void run(Runnable launch, Runnable failed) {
    try {
      launch.run();
    } catch (RuntimeException ignored) {
      failed.run();
    }
  }
}
