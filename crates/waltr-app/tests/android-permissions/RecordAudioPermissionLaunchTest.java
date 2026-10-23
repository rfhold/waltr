package dev.waltr.app;

import java.util.ArrayList;
import java.util.List;

public final class RecordAudioPermissionLaunchTest {
  private static final class Host {
    Long ticket;
    int launches;
    final List<String> replies = new ArrayList<>();

    void request(long requested, Runnable launch) {
      if (ticket != null) throw new AssertionError("dialog slot still occupied");
      ticket = requested;
      RecordAudioPermissionLaunch.run(() -> {
        launches++;
        launch.run();
      }, () -> {
        if (Long.valueOf(requested).equals(ticket)) {
          ticket = null;
          replies.add(requested + ":denied");
        }
      });
    }

    void complete(boolean granted) {
      Long original = ticket;
      ticket = null;
      if (original != null) replies.add(original + (granted ? ":granted" : ":denied"));
    }
  }

  public static void main(String[] args) {
    Host host = new Host();
    host.request(1, () -> { throw new SecurityException("must not escape or enter a reply"); });
    require(host.ticket == null, "SecurityException must release the original slot");
    require(host.replies.equals(java.util.Arrays.asList("1:denied")), "one sanitized terminal reply");
    host.request(2, () -> {});
    require(host.launches == 2, "the next request must launch");
    require(Long.valueOf(2).equals(host.ticket), "replacement retains its own identity");
    host.complete(true);
    host.complete(true);
    require(host.replies.equals(java.util.Arrays.asList("1:denied", "2:granted")), "replacement completes only once");

    for (RuntimeException failure : new RuntimeException[] {
      new IllegalStateException(), new IllegalArgumentException(), new SecurityException()
    }) {
      int[] failures = {0};
      RecordAudioPermissionLaunch.run(() -> { throw failure; }, () -> failures[0]++);
      require(failures[0] == 1, "each launch RuntimeException receives one terminal reply");
    }

    Host replaced = new Host();
    replaced.request(3, () -> {
      replaced.ticket = 4L;
      throw new SecurityException();
    });
    require(Long.valueOf(4).equals(replaced.ticket), "failure cannot clear another ticket");
    require(replaced.replies.isEmpty(), "failure cannot reply for another ticket");

    AssertionError fatal = new AssertionError("fatal Error must escape");
    try {
      RecordAudioPermissionLaunch.run(() -> { throw fatal; }, () -> {
        throw new AssertionError("must not catch Error");
      });
      throw new AssertionError("Error did not escape");
    } catch (AssertionError observed) {
      require(observed == fatal, "preserve the original Error");
    }

    RuntimeException replyFailure = new RuntimeException("outside launch boundary");
    try {
      RecordAudioPermissionLaunch.run(() -> { throw new SecurityException(); }, () -> {
        throw replyFailure;
      });
      throw new AssertionError("reply exception did not escape");
    } catch (RuntimeException observed) {
      require(observed == replyFailure, "do not catch unrelated reply failures");
    }
  }

  private static void require(boolean condition, String message) {
    if (!condition) throw new AssertionError(message);
  }
}
