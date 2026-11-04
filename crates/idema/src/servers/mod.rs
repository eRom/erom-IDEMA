//! Les serveurs de dev des projets (P4 jalon 2) : lecture de la configuration, relevé des ports, état des lignes.
//! Tous sont purs (aucune fenêtre, aucune entrée-sortie hors du `Scanner` et des fonctions d'arrêt) sauf `model`, l'entité GPUI qui les assemble.

pub(crate) mod launch;
pub(crate) mod model;
pub(crate) mod registry;
pub(crate) mod scan;
pub(crate) mod session_agents;
pub(crate) mod stop;
pub(crate) mod tail;
