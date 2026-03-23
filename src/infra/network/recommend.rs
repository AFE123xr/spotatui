use super::Network;
use crate::core::app::{ActiveBlock, RouteId, TrackTableContext};
use anyhow::anyhow;
use rspotify::model::{
  enums::Country,
  idtypes::{ArtistId, TrackId},
  track::FullTrack,
  Market,
};
use rspotify::prelude::*;

pub trait RecommendationNetwork {
  async fn get_recommendations_for_seed(
    &mut self,
    seed_artists: Option<Vec<ArtistId<'static>>>,
    seed_tracks: Option<Vec<TrackId<'static>>>,
    first_track: Box<Option<FullTrack>>,
    country: Option<Country>,
  );
  async fn get_recommendations_for_track_id(
    &mut self,
    track_id: TrackId<'static>,
    country: Option<Country>,
  );
}

impl RecommendationNetwork for Network {
  async fn get_recommendations_for_seed(
    &mut self,
    seed_artists: Option<Vec<ArtistId<'static>>>,
    seed_tracks: Option<Vec<TrackId<'static>>>,
    first_track: Box<Option<FullTrack>>,
    country: Option<Country>,
  ) {
    let _market = country.map(Market::Country);
    let limit = self.large_search_limit;

    match self
      .spotify
      .recommendations(
        std::iter::empty(),
        seed_artists,
        None::<Vec<&str>>, // seed_genres
        seed_tracks,
        _market,
        Some(limit),
      )
      .await
    {
      Ok(recommendations) => {
        let mut app = self.app.lock().await;
        // Convert SimplifiedTrack to FullTrack (best effort)
        // SimplifiedTrack doesn't have album field which FullTrack needs.
        // This is tricky. Recommendations usually return SimplifiedTracks.
        // We probably need to fetch FullTracks or fake it.
        // For now, let's map what we can and use a dummy album or fail.
        // Better: use spotify.tracks() to fetch full details if possible.

        // Actually, we can fetch the full tracks using the IDs.
        let track_ids: Vec<TrackId> = recommendations
          .tracks
          .iter()
          .filter_map(|t| t.id.clone())
          .collect();

        let mut full_tracks = Vec::new();
        if !track_ids.is_empty() {
          for id in &track_ids {
            if let Ok(track) = self.spotify.track(id.clone(), None).await {
              full_tracks.push(track);
            }
          }
        }

        app.track_table.tracks = full_tracks;

        // Prepend the seed track if available so user knows context
        if let Some(track) = *first_track {
          app.track_table.tracks.insert(0, track);
        }
        app.track_table.context = Some(TrackTableContext::RecommendedTracks);
        app.push_navigation_stack(RouteId::Recommendations, ActiveBlock::TrackTable);
      }
      Err(e) => {
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_recommendations_for_track_id(
    &mut self,
    track_id: TrackId<'static>,
    country: Option<Country>,
  ) {
    let seed_tracks = Some(vec![track_id.clone()]);
    let first_track: Box<Option<FullTrack>> = Box::new(None);

    self
      .get_recommendations_for_seed(None, seed_tracks, first_track, country)
      .await;
  }
}
