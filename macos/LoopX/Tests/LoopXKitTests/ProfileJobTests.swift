import Foundation
import Testing
@testable import LoopXKit

/// Project profile builds, section refreshes and the weekly sync, as the snapshot lists them
/// (`profile:<alias>` and `profile-sync`, with `kind`, `step`, `steps`, `alias`, `step_name`).
@Suite struct ProfileJobTests {
    static let json = """
    {"version": 1, "app_state": "running", "loops": {
      "profile:web": {"state": "running", "updated_at": "2026-11-12T10:00:00+00:00",
        "current_issue": "Profile · web", "current_step": "Asking the AI · step 3 of 5",
        "kind": "build", "section": null, "alias": "web", "step": 3, "steps": 5,
        "step_code": "ai", "step_name": "Asking the AI"},
      "profile-sync": {"state": "running", "current_issue": "Profile sync", "current_step": "web · project 2 of 3",
        "kind": "sync", "alias": "web", "step": 2, "steps": 3, "step_code": "project", "step_name": "web"}
    }}
    """

    @Test func theNewFieldsDecode() throws {
        let s = try Snapshot.decode(Data(Self.json.utf8))
        let web = try #require(s.loops["profile:web"])
        #expect(web.kind == "build")
        #expect(web.alias == "web")
        #expect(web.step == 3)
        #expect(web.steps == 5)
        #expect(web.stepName == "Asking the AI")
        #expect(s.loops["profile-sync"]?.kind == "sync")
    }

    @Test func aProfileJobReadsAsItsProjectAndStep() throws {
        let s = try Snapshot.decode(Data(Self.json.utf8))
        let tasks = TaskNotifications.tasks(in: s)
        let web = try #require(tasks.first { $0.name == "profile:web" })
        #expect(web.title == "Profile · web")
        #expect(web.detail == "3/5 Asking the AI")
        #expect(web.state == .running)
        let sync = try #require(tasks.first { $0.name == "profile-sync" })
        #expect(sync.title == "Profile sync")
        #expect(sync.detail == "2/3 web")
        let one = Snapshot(appState: .running, loops: ["profile:web": s.loops["profile:web"]!])
        let notch = NotchState.make(from: one, now: Date())
        #expect(notch.title == "Profile · web running")
        #expect(notch.subtitle == "3/5 Asking the AI")
    }

    @Test func withoutTheStepFieldsItFallsBackToTheIssueAndStepText() throws {
        let old = """
        {"loops": {"profile:web": {"state": "running", "current_issue": "Profile · web",
          "current_step": "Asking the AI · step 3 of 5", "step": "three", "steps": null}}}
        """
        let s = try Snapshot.decode(Data(old.utf8))
        let l = try #require(s.loops["profile:web"], "a malformed new field does not drop the entry")
        #expect(l.step == nil)
        let t = try #require(TaskNotifications.tasks(in: s).first)
        #expect(t.detail == "Profile · web · Asking the AI · step 3 of 5")
    }

    @Test func profileJobsAreNotRunnableLoopsAndPostNoNotifications() throws {
        #expect(LoopNames.isCreatorJob("profile:web"))
        #expect(LoopNames.isCreatorJob("profile-sync"))
        #expect(LoopNames.symbol("profile:web") == "person.text.rectangle")
        let running = try Snapshot.decode(Data(Self.json.utf8))
        let idle = Snapshot()
        #expect(TaskNotifications.changes(from: idle, to: running).isEmpty)
        #expect(TaskNotifications.changes(from: running, to: idle).isEmpty)
    }
}
