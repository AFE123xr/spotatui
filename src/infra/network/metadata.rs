use super::requests::spotify_get_typed_compat_for;
use super::Network;
use crate::core::app::{
  ActiveBlock, Artist, ArtistBlock, EpisodeTableContext, RouteId, ScrollableResultPages,
  SelectedFullShow, SelectedShow,
};
use anyhow::anyhow;
use futures::stream::StreamExt;
use log::{debug, error, info};
use rspotify::model::{
  album::SimplifiedAlbum,
  artist::FullArtist,
  enums::Country,
  idtypes::{AlbumId, ArtistId, ShowId, TrackId},
  page::Page,
  show::SimplifiedShow,
  Market,
};
use rspotify::prelude::*;
use tokio::try_join;

pub trait MetadataNetwork {
  async fn get_artist(
    &mut self,
    artist_id: ArtistId<'static>,
    input_artist_name: String,
    country: Option<Country>,
  );
  async fn get_album_tracks(&mut self, album: Box<SimplifiedAlbum>);
  async fn get_album(&mut self, album_id: AlbumId<'static>);
  async fn get_show_episodes(&mut self, show: Box<SimplifiedShow>);
  async fn get_show(&mut self, show_id: ShowId<'static>);
  async fn get_current_show_episodes(&mut self, show_id: ShowId<'static>, offset: Option<u32>);
  async fn get_followed_artists(&mut self, after: Option<ArtistId<'static>>);
  async fn user_unfollow_artists(&mut self, artist_ids: Vec<ArtistId<'static>>);
  async fn user_follow_artists(&mut self, artist_ids: Vec<ArtistId<'static>>);
  async fn user_artist_check_follow(&mut self, artist_ids: Vec<ArtistId<'static>>);
  async fn set_artists_to_table(&mut self, artists: Vec<FullArtist>);
  #[allow(dead_code)]
  async fn get_album_for_track(&mut self, track_id: TrackId<'static>);
}

impl MetadataNetwork for Network {
  async fn get_artist(
    &mut self,
    artist_id: ArtistId<'static>,
    input_artist_name: String,
    country: Option<Country>,
  ) {
    let artist_id_str = artist_id.id().to_string();
    debug!(
      "get_artist: id={} name={} country={:?}",
      artist_id_str, input_artist_name, country
    );
    let market = country.map(Market::Country);
    let top_tracks_req = self.spotify.artist_top_tracks(artist_id.clone(), market);
    // rspotify 0.14 artist_related_artists is not deprecated or we suppress
    #[allow(deprecated)]
    let related_artists_req = self.spotify.artist_related_artists(artist_id.clone());
    let albums_req = self.spotify.artist_albums(artist_id.clone(), None, market);

    // Note: artist_albums returns a Paginator (Stream), we need to collect it.
    // However try_join! expects futures. We wrap stream collection in async block.
    let albums_fut = async {
      let stream = albums_req;
      let items: Vec<_> = stream.collect().await;

      // Flatten results and collect SimplifiedAlbum
      let albums: Vec<SimplifiedAlbum> = items.into_iter().filter_map(|r| r.ok()).collect();

      // Wrap in Page
      Ok(Page {
        items: albums,
        href: String::new(),
        limit: 50,
        next: None,
        offset: 0,
        previous: None,
        total: 0,
      })
    };

    debug!(
      "get_artist: dispatching top_tracks, related_artists and albums requests for {}",
      artist_id_str
    );
    let res = try_join!(top_tracks_req, related_artists_req, albums_fut);

    match res {
      Ok((top_tracks, related_artists, albums)) => {
        let mut app = self.app.lock().await;
        app.artist = Some(Artist {
          artist_id: artist_id_str,
          artist_name: input_artist_name,
          albums,
          related_artists,
          top_tracks,
          selected_album_index: 0,
          selected_related_artist_index: 0,
          selected_top_track_index: 0,
          artist_selected_block: ArtistBlock::TopTracks,
          artist_hovered_block: ArtistBlock::TopTracks,
        });
        app.push_navigation_stack(RouteId::Artist, ActiveBlock::ArtistBlock);
        info!("get_artist: populated app.artist for {}", artist_id.id());
      }
      Err(e) => {
        error!("get_artist failed for {}: {:?}", artist_id_str, e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_album_tracks(&mut self, album: Box<SimplifiedAlbum>) {
    let album_id = album.id.clone();
    if let Some(id) = album_id {
      debug!("get_album_tracks: album_id={}", id.id());
      let path = format!("albums/{}/tracks", id.id());
      // TODO: Handle pagination for albums with > 50 tracks
      match spotify_get_typed_compat_for::<Page<rspotify::model::track::SimplifiedTrack>>(
        &self.spotify,
        &path,
        &[("limit", "50".to_string()), ("offset", "0".to_string())],
      )
      .await
      {
        Ok(tracks) => {
          let mut app = self.app.lock().await;
          app.selected_album_simplified = Some(crate::core::app::SelectedAlbum {
            album: *album,
            tracks,
            selected_index: 0,
          });
          app.album_table_context = crate::core::app::AlbumTableContext::Simplified;
          app.push_navigation_stack(RouteId::AlbumTracks, ActiveBlock::AlbumTracks);
          info!("get_album_tracks: loaded tracks for album {}", id.id());
        }
        Err(e) => self.handle_error(anyhow!(e)).await,
      }
    }
  }

  async fn get_album(&mut self, album_id: AlbumId<'static>) {
    debug!("get_album: album_id={}", album_id.id());
    match self.spotify.album(album_id.clone(), None).await {
      Ok(album) => {
        let mut app = self.app.lock().await;
        app.selected_album_full = Some(crate::core::app::SelectedFullAlbum {
          album,
          selected_index: 0,
        });
        app.album_table_context = crate::core::app::AlbumTableContext::Full;
        app.push_navigation_stack(RouteId::AlbumTracks, ActiveBlock::AlbumTracks);
        info!("get_album: loaded full album {}", album_id.id());
      }
      Err(e) => {
        error!("get_album failed for {}: {:?}", album_id.id(), e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_show_episodes(&mut self, show: Box<SimplifiedShow>) {
    let show_id = show.id.clone();
    debug!("get_show_episodes: show_id={}", show_id.id());
    let path = format!("shows/{}/episodes", show_id.id());
    let query = vec![
      ("limit", self.large_search_limit.to_string()),
      ("offset", "0".to_string()),
    ];
    match spotify_get_typed_compat_for::<Page<rspotify::model::show::SimplifiedEpisode>>(
      &self.spotify,
      &path,
      &query,
    )
    .await
    {
      Ok(episodes) => {
        if !episodes.items.is_empty() {
          debug!(
            "get_show_episodes: fetched {} episodes for show {}",
            episodes.items.len(),
            show_id.id()
          );
          let mut app = self.app.lock().await;
          app.library.show_episodes = ScrollableResultPages::new();
          app.library.show_episodes.add_pages(episodes);

          app.selected_show_simplified = Some(SelectedShow { show: *show });

          app.episode_table_context = EpisodeTableContext::Simplified;

          app.push_navigation_stack(RouteId::PodcastEpisodes, ActiveBlock::EpisodeTable);
        } else {
          debug!(
            "get_show_episodes: no episodes returned for show {}",
            show_id.id()
          );
        }
      }
      Err(e) => {
        error!("get_show_episodes failed for {}: {:?}", show_id.id(), e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_show(&mut self, show_id: ShowId<'static>) {
    let path = format!("shows/{}", show_id.id());
    debug!("get_show: show_id={}", show_id.id());
    match spotify_get_typed_compat_for::<rspotify::model::show::FullShow>(&self.spotify, &path, &[])
      .await
    {
      Ok(show) => {
        let selected_show = SelectedFullShow { show };

        let mut app = self.app.lock().await;

        app.selected_show_full = Some(selected_show);

        app.episode_table_context = EpisodeTableContext::Full;
        app.push_navigation_stack(RouteId::PodcastEpisodes, ActiveBlock::EpisodeTable);
        info!("get_show: loaded full show {}", show_id.id());
      }
      Err(e) => {
        error!("get_show failed for {}: {:?}", show_id.id(), e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_current_show_episodes(&mut self, show_id: ShowId<'static>, offset: Option<u32>) {
    let path = format!("shows/{}/episodes", show_id.id());
    debug!(
      "get_current_show_episodes: show_id={} offset={:?}",
      show_id.id(),
      offset
    );
    let mut query = vec![("limit", self.large_search_limit.to_string())];
    if let Some(offset) = offset {
      query.push(("offset", offset.to_string()));
    }

    match spotify_get_typed_compat_for::<Page<rspotify::model::show::SimplifiedEpisode>>(
      &self.spotify,
      &path,
      &query,
    )
    .await
    {
      Ok(episodes) => {
        if !episodes.items.is_empty() {
          debug!(
            "get_current_show_episodes: adding {} episodes for show {}",
            episodes.items.len(),
            show_id.id()
          );
          let mut app = self.app.lock().await;
          app.library.show_episodes.add_pages(episodes);
        }
      }
      Err(e) => {
        error!(
          "get_current_show_episodes failed for {}: {:?}",
          show_id.id(),
          e
        );
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_followed_artists(&mut self, after: Option<ArtistId<'static>>) {
    let limit = self.large_search_limit;
    let after_id = after.as_ref().map(|id| id.id());
    debug!("get_followed_artists: after={:?} limit={}", after_id, limit);
    match self
      .spotify
      .current_user_followed_artists(after_id, Some(limit))
      .await
    {
      Ok(artists_page) => {
        debug!(
          "get_followed_artists: got page with {} artists",
          artists_page.items.len()
        );
        let mut app = self.app.lock().await;
        app.library.saved_artists.add_pages(artists_page);
      }
      Err(e) => {
        error!("get_followed_artists failed: {:?}", e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn user_unfollow_artists(&mut self, artist_ids: Vec<ArtistId<'static>>) {
    debug!(
      "user_unfollow_artists: ids={:?}",
      artist_ids.iter().map(|id| id.id()).collect::<Vec<_>>()
    );
    match self.spotify.user_unfollow_artists(artist_ids).await {
      Ok(_) => {
        info!("user_unfollow_artists: success");
      }
      Err(e) => {
        error!("user_unfollow_artists failed: {:?}", e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn user_follow_artists(&mut self, artist_ids: Vec<ArtistId<'static>>) {
    debug!(
      "user_follow_artists: ids={:?}",
      artist_ids.iter().map(|id| id.id()).collect::<Vec<_>>()
    );
    match self.spotify.user_follow_artists(artist_ids).await {
      Ok(_) => {
        info!("user_follow_artists: success");
      }
      Err(e) => {
        error!("user_follow_artists failed: {:?}", e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn user_artist_check_follow(&mut self, artist_ids: Vec<ArtistId<'static>>) {
    debug!(
      "user_artist_check_follow: ids={:?}",
      artist_ids.iter().map(|id| id.id()).collect::<Vec<_>>()
    );
    match self
      .spotify
      .user_artist_check_follow(artist_ids.clone())
      .await
    {
      Ok(is_following) => {
        let mut app = self.app.lock().await;
        for (i, is_following) in is_following.iter().enumerate() {
          if *is_following {
            app
              .followed_artist_ids_set
              .insert(artist_ids[i].id().to_string());
          }
        }
        info!(
          "user_artist_check_follow: {} followed",
          app.followed_artist_ids_set.len()
        );
      }
      Err(e) => {
        error!("user_artist_check_follow failed: {:?}", e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn set_artists_to_table(&mut self, artists: Vec<FullArtist>) {
    debug!("set_artists_to_table: {} artists", artists.len());
    let mut app = self.app.lock().await;
    app.artists = artists;
  }

  async fn get_album_for_track(&mut self, track_id: TrackId<'static>) {
    debug!("get_album_for_track: track_id={}", track_id.id());
    match self.spotify.track(track_id.clone(), None).await {
      Ok(track) => {
        // FullTrack.album is SimplifiedAlbum (not Option) in rspotify 0.14
        let album = track.album;
        info!(
          "get_album_for_track: resolved album id={}",
          album.id.as_ref().map(|i| i.id()).unwrap_or_default()
        );
        self.get_album_tracks(Box::new(album)).await;
      }
      Err(e) => {
        error!("get_album_for_track failed for {}: {:?}", track_id.id(), e);
        self.handle_error(anyhow!(e)).await;
      }
    }
  }
}
