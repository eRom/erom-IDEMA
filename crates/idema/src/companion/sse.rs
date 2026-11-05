//! Le flux de la flotte : un événement `fleet` à chaque nouvel instantané, regroupé sur une courte fenêtre. Le premier
//! événement porte l'instantané du moment de la connexion : un client qui s'abonne après avoir lu `fleet` ne rate
//! jamais un changement survenu entre les deux.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event;
use futures::{Stream, StreamExt};
use tokio::sync::watch;

use super::fleet::Fleet;

/// Les publications qui arrivent dans cette fenêtre n'en font qu'un événement, le dernier instantané.
pub const COALESCE: Duration = Duration::from_millis(250);

/// Les instantanés à pousser : celui d'aujourd'hui tout de suite, puis le dernier de chaque salve de changements,
/// `window` après le premier changement de la salve. Le flux finit quand l'émetteur disparaît.
pub fn fleet_updates(
    fleet: watch::Receiver<Arc<Fleet>>,
    window: Duration,
) -> impl Stream<Item = Arc<Fleet>> {
    futures::stream::unfold((fleet, false), move |(mut fleet, started)| async move {
        if started {
            fleet.changed().await.ok()?;
            tokio::time::sleep(window).await;
        }
        let latest = Arc::clone(&fleet.borrow_and_update());
        Some((latest, (fleet, true)))
    })
}

/// L'événement SSE d'un instantané. Le JSON compact n'a pas de saut de ligne : il tient sur une ligne `data:`.
fn fleet_event(fleet: &Fleet) -> Option<Event> {
    let json = serde_json::to_string(fleet).ok()?;
    Some(Event::default().event("fleet").data(json))
}

/// Le flux d'événements `fleet` prêt pour `Sse`. Un instantané qu'on ne saurait pas écrire ferme le flux : le client
/// se reconnecte et reçoit l'instantané suivant.
pub fn fleet_events(
    fleet: watch::Receiver<Arc<Fleet>>,
    window: Duration,
) -> impl Stream<Item = Result<Event, Infallible>> {
    fleet_updates(fleet, window)
        .map(|latest| fleet_event(&latest))
        .take_while(|event| std::future::ready(event.is_some()))
        .filter_map(|event| std::future::ready(event.map(Ok)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::time::Instant;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    fn numbered(badge: usize) -> Arc<Fleet> {
        Arc::new(Fleet {
            badge,
            ..Fleet::default()
        })
    }

    #[test]
    fn events_coalesce_bursts() {
        runtime().block_on(async {
            let (sender, receiver) = watch::channel(numbered(0));
            let mut updates = pin!(fleet_updates(receiver, Duration::from_millis(100)));

            // Le premier élément est l'instantané de la connexion.
            assert_eq!(updates.next().await.unwrap().badge, 0);

            // Dix publications en 50 ms : un seul événement, le dernier.
            for badge in 1..=10 {
                sender.send(numbered(badge)).unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let burst = updates.next().await.unwrap();
            assert_eq!(burst.badge, 10);
            let quiet = tokio::time::timeout(Duration::from_millis(400), updates.next()).await;
            assert!(quiet.is_err(), "the burst produced exactly one event");

            // Un changement ultérieur repart pour un nouvel événement, après la fenêtre.
            let sent = Instant::now();
            sender.send(numbered(11)).unwrap();
            assert_eq!(updates.next().await.unwrap().badge, 11);
            assert!(sent.elapsed() >= Duration::from_millis(100));
        });
    }

    #[test]
    fn the_stream_ends_when_the_publisher_goes_away() {
        runtime().block_on(async {
            let (sender, receiver) = watch::channel(numbered(0));
            let mut updates = pin!(fleet_updates(receiver, Duration::from_millis(10)));
            assert!(updates.next().await.is_some());
            drop(sender);
            assert!(updates.next().await.is_none());
        });
    }

    #[test]
    fn a_change_published_before_the_connection_is_in_the_first_event() {
        runtime().block_on(async {
            let (sender, receiver) = watch::channel(numbered(0));
            sender.send(numbered(7)).unwrap();
            let mut updates = pin!(fleet_updates(receiver, Duration::from_millis(10)));
            assert_eq!(updates.next().await.unwrap().badge, 7);
            let quiet = tokio::time::timeout(Duration::from_millis(100), updates.next()).await;
            assert!(quiet.is_err(), "the same snapshot is not sent twice");
        });
    }
}
