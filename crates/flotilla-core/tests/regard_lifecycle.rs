use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use flotilla_core::regard_lifecycle::{Clock, RegardLifecycle, SurfaceGestureOutcome};
use flotilla_protocol::{PrincipalRef, ResourceRef, SurfaceCharacter, SurfaceDeclaration};
use flotilla_resources::{
    apply_status_patch, InMemoryBackend, InputMeta, Regard, RegardExpiryPolicy, RegardSource, RegardSpec, RegardStatusPatch,
    ResourceBackend,
};

#[derive(Debug)]
struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    fn new(now: DateTime<Utc>) -> Self {
        Self { now: Mutex::new(now) }
    }

    fn advance(&self, duration: Duration) {
        let mut now = self.now.lock().expect("manual clock lock");
        *now += duration;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("manual clock lock")
    }
}

fn principal() -> PrincipalRef {
    PrincipalRef::implicit_for_namespace("flotilla")
}

fn target() -> ResourceRef {
    ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "demo")
}

#[tokio::test]
async fn focal_observation_decays_only_after_the_target_has_left_every_surface() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let clock = Arc::new(ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).unwrap()));
    let lifecycle = RegardLifecycle::new(backend.clone(), clock.clone(), Duration::seconds(30));
    let surface_id = uuid::Uuid::new_v4();

    lifecycle.connect_surface(surface_id, SurfaceDeclaration { principal_ref: principal(), character: SurfaceCharacter::Focal });
    lifecycle.observe_focus(surface_id, vec![target()]).await.expect("observe focused convoy");

    let regards = backend.using::<Regard>("flotilla");
    let created = regards.list().await.expect("list created regard");
    assert_eq!(created.items.len(), 1);
    assert_eq!(created.items[0].spec.target, target());
    assert_eq!(created.items[0].status.as_ref().and_then(|status| status.refreshed_at), Some(clock.now()));

    clock.advance(Duration::seconds(5));
    lifecycle.observe_focus(surface_id, Vec::new()).await.expect("leave convoy focus");
    clock.advance(Duration::seconds(29));
    lifecycle.expire_due("flotilla").await.expect("sweep before deadline");
    assert_eq!(regards.list().await.expect("list live regard").items.len(), 1);

    clock.advance(Duration::seconds(2));
    lifecycle.expire_due("flotilla").await.expect("sweep after deadline");
    assert!(regards.list().await.expect("list expired regards").items.is_empty());
}

#[tokio::test]
async fn disconnecting_the_last_focal_surface_starts_the_decay_window() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let clock = Arc::new(ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).unwrap()));
    let lifecycle = RegardLifecycle::new(backend.clone(), clock.clone(), Duration::seconds(30));
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let declaration = SurfaceDeclaration { principal_ref: principal(), character: SurfaceCharacter::Focal };

    lifecycle.connect_surface(first, declaration.clone());
    lifecycle.connect_surface(second, declaration);
    assert_eq!(lifecycle.emit_expressed_for_surface(first, &target()).await.expect("first surface focus"), SurfaceGestureOutcome::Handled);
    lifecycle.observe_focus(second, vec![target()]).await.expect("second surface focus");

    lifecycle.disconnect_surface(first).await.expect("disconnect first surface");
    clock.advance(Duration::seconds(31));
    lifecycle.expire_due("flotilla").await.expect("sweep while second surface remains focused");
    assert_eq!(backend.using::<Regard>("flotilla").list().await.expect("list live regard").items.len(), 1);

    lifecycle.disconnect_surface(second).await.expect("disconnect last surface");
    clock.advance(Duration::seconds(29));
    lifecycle.expire_due("flotilla").await.expect("sweep before disconnect deadline");
    assert_eq!(backend.using::<Regard>("flotilla").list().await.expect("list before deadline").items.len(), 1);

    clock.advance(Duration::seconds(2));
    lifecycle.expire_due("flotilla").await.expect("sweep after disconnect deadline");
    assert!(backend.using::<Regard>("flotilla").list().await.expect("list expired regards").items.is_empty());
}

#[tokio::test]
async fn implicit_emission_records_its_policy_and_visible_decay_window() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let clock = Arc::new(ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).unwrap()));
    let lifecycle = RegardLifecycle::new(backend.clone(), clock, Duration::seconds(45));

    lifecycle.emit_implicit(&principal(), &target(), "convoy-dispatch").await.expect("emit implicit regard");

    let regards = backend.using::<Regard>("flotilla").list().await.expect("list implicit regard");
    assert_eq!(regards.items.len(), 1);
    assert_eq!(regards.items[0].spec.source, RegardSource::Implicit { policy: "convoy-dispatch".to_string() });
    assert_eq!(regards.items[0].spec.expiry, RegardExpiryPolicy::Decaying { expires_after_seconds: 45 });
}

#[tokio::test]
async fn ambient_observations_emit_nothing_and_pins_never_expire() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let clock = Arc::new(ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).unwrap()));
    let lifecycle = RegardLifecycle::new(backend.clone(), clock.clone(), Duration::seconds(30));
    let ambient = uuid::Uuid::new_v4();
    lifecycle.connect_surface(ambient, SurfaceDeclaration { principal_ref: principal(), character: SurfaceCharacter::Ambient });

    lifecycle.observe_focus(ambient, vec![target()]).await.expect("ambient observation");
    assert_eq!(
        lifecycle.emit_expressed_for_surface(ambient, &target()).await.expect("ambient explicit gesture"),
        SurfaceGestureOutcome::Handled
    );
    let resolver = backend.using::<Regard>("flotilla");
    assert!(resolver.list().await.expect("list after ambient observation").items.is_empty());

    let pinned = resolver
        .create(
            &InputMeta::builder().name("pinned-demo".to_string()).build(),
            &RegardSpec::builder()
                .principal_ref(principal())
                .target(target())
                .source(RegardSource::Expressed)
                .expiry(RegardExpiryPolicy::Pin)
                .build(),
        )
        .await
        .expect("create pin");
    apply_status_patch(&resolver, &pinned.metadata.name, &RegardStatusPatch::Refresh { as_of: clock.now() }).await.expect("stamp pin");

    clock.advance(Duration::days(365));
    lifecycle.expire_due("flotilla").await.expect("sweep pins");
    assert_eq!(resolver.list().await.expect("list pins").items.len(), 1);
}

#[tokio::test]
async fn expressed_focus_promotes_an_existing_implicit_regard_without_duplication() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let clock = Arc::new(ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).unwrap()));
    let lifecycle = RegardLifecycle::new(backend.clone(), clock.clone(), Duration::seconds(30));

    lifecycle.emit_implicit(&principal(), &target(), "convoy-dispatch").await.expect("implicit regard");
    clock.advance(Duration::seconds(10));
    lifecycle.emit_expressed(&principal(), &target()).await.expect("express regard");

    let regards = backend.using::<Regard>("flotilla").list().await.expect("list promoted regard");
    assert_eq!(regards.items.len(), 1);
    assert_eq!(regards.items[0].spec.source, RegardSource::Expressed);
    assert_eq!(regards.items[0].status.as_ref().and_then(|status| status.refreshed_at), Some(clock.now()));
}

#[tokio::test]
async fn mesh_resident_heartbeats_prevent_another_daemon_from_expiring_remote_focus() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let clock = Arc::new(ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).unwrap()));
    let sweeper = RegardLifecycle::new(backend.clone(), clock.clone(), Duration::seconds(30));
    let observer = RegardLifecycle::new(backend.clone(), clock.clone(), Duration::seconds(30));
    let surface = uuid::Uuid::new_v4();
    observer.connect_surface(surface, SurfaceDeclaration { principal_ref: principal(), character: SurfaceCharacter::Focal });
    observer.observe_focus(surface, vec![target()]).await.expect("remote focus");

    clock.advance(Duration::seconds(29));
    observer.refresh_focused().await.expect("remote heartbeat");
    clock.advance(Duration::seconds(2));
    sweeper.expire_due("flotilla").await.expect("sweep after original deadline");
    assert_eq!(backend.using::<Regard>("flotilla").list().await.expect("live regard").items.len(), 1);

    observer.disconnect_surface(surface).await.expect("remote detach");
    clock.advance(Duration::seconds(31));
    sweeper.expire_due("flotilla").await.expect("sweep after detach deadline");
    assert!(backend.using::<Regard>("flotilla").list().await.expect("expired regard").items.is_empty());
}

#[tokio::test]
async fn expiry_sweeps_target_namespaces_observed_by_cross_host_gestures() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let clock = Arc::new(ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).unwrap()));
    let lifecycle = RegardLifecycle::new(backend.clone(), clock.clone(), Duration::seconds(30));
    let remote_target = ResourceRef::new("flotilla.work/v1", "Convoy", "remote-host", "demo");

    lifecycle.emit_expressed(&principal(), &remote_target).await.expect("emit cross-host regard");
    clock.advance(Duration::seconds(31));
    lifecycle.expire_due("flotilla").await.expect("sweep every observed namespace");

    assert!(backend.using::<Regard>("remote-host").list().await.expect("list remote regards").items.is_empty());
}
