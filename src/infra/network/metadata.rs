use super::requests::spotify_get_typed_compat_for;
use super::Network;
use crate::core::app::{
  ActiveBlock, Artist, ArtistBlock, EpisodeTableContext, RouteId, ScrollableResultPages,
  SelectedFullShow, SelectedShow,
};
use anyhow::anyhow;
use futures::stream::StreamExt;
use rspotify::model::{
  album::SimplifiedAlbum,
  artist::FullArtist,
  enums::Country,
  idtypes::{AlbumId, ArtistId, LibraryId, ShowId, TrackId},
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
    let market = country.map(Market::Country);
    #[allow(deprecated)]
    let top_tracks_req = self.spotify.artist_top_tracks(artist_id.clone(), market);
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
      }
      Err(e) => {
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_album_tracks(&mut self, album: Box<SimplifiedAlbum>) {
    let album_id = album.id.clone();
    if let Some(id) = album_id {
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
        }
        Err(e) => self.handle_error(anyhow!(e)).await,
      }
    }
  }

  async fn get_album(&mut self, album_id: AlbumId<'static>) {
    match self.spotify.album(album_id, None).await {
      Ok(album) => {
        let mut app = self.app.lock().await;
        app.selected_album_full = Some(crate::core::app::SelectedFullAlbum {
          album,
          selected_index: 0,
        });
        app.album_table_context = crate::core::app::AlbumTableContext::Full;
        app.push_navigation_stack(RouteId::AlbumTracks, ActiveBlock::AlbumTracks);
      }
      Err(e) => self.handle_error(anyhow!(e)).await,
    }
  }

  async fn get_show_episodes(&mut self, show: Box<SimplifiedShow>) {
    let show_id = show.id.clone();
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
          let mut app = self.app.lock().await;
          app.library.show_episodes = ScrollableResultPages::new();
          app.library.show_episodes.add_pages(episodes);

          app.selected_show_simplified = Some(SelectedShow { show: *show });

          app.episode_table_context = EpisodeTableContext::Simplified;

          app.push_navigation_stack(RouteId::PodcastEpisodes, ActiveBlock::EpisodeTable);
        }
      }
      Err(e) => {
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_show(&mut self, show_id: ShowId<'static>) {
    let path = format!("shows/{}", show_id.id());
    match spotify_get_typed_compat_for::<rspotify::model::show::FullShow>(&self.spotify, &path, &[])
      .await
    {
      Ok(show) => {
        let selected_show = SelectedFullShow { show };

        let mut app = self.app.lock().await;

        app.selected_show_full = Some(selected_show);

        app.episode_table_context = EpisodeTableContext::Full;
        app.push_navigation_stack(RouteId::PodcastEpisodes, ActiveBlock::EpisodeTable);
      }
      Err(e) => {
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_current_show_episodes(&mut self, show_id: ShowId<'static>, offset: Option<u32>) {
    let path = format!("shows/{}/episodes", show_id.id());
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
          let mut app = self.app.lock().await;
          app.library.show_episodes.add_pages(episodes);
        }
      }
      Err(e) => {
        self.handle_error(anyhow!(e)).await;
      }
    }
  }

  async fn get_followed_artists(&mut self, after: Option<ArtistId<'static>>) {
    let limit = self.large_search_limit;
    let after_id = after.as_ref().map(|id| id.id());
    match self
      .spotify
      .current_user_followed_artists(after_id, Some(limit))
      .await
    {
      Ok(artists_page) => {
        let mut app = self.app.lock().await;
        // Error: add_pages expects CursorBasedPage<FullArtist>, but items is Vec<FullArtist>
        // Check add_pages definition. It takes T.
        // artists_page is CursorBasedPage<FullArtist>.
        // So we should pass the whole page if add_pages handles it.
        // App definition: saved_artists: ScrollableResultPages<CursorBasedPage<FullArtist>>
        app.library.saved_artists.add_pages(artists_page);
      }
      Err(e) => self.handle_error(anyhow!(e)).await,
    }
  }

  async fn user_unfollow_artists(&mut self, artist_ids: Vec<ArtistId<'static>>) {
    match self
      .spotify
      .library_remove(artist_ids.into_iter().map(LibraryId::Artist))
      .await
    {
      Ok(_) => {
        // Handled
      }
      Err(e) => self.handle_error(anyhow!(e)).await,
    }
  }

  async fn user_follow_artists(&mut self, artist_ids: Vec<ArtistId<'static>>) {
    match self
      .spotify
      .library_add(artist_ids.into_iter().map(LibraryId::Artist))
      .await
    {
      Ok(_) => {
        // Handled
      }
      Err(e) => self.handle_error(anyhow!(e)).await,
    }
  }

  async fn user_artist_check_follow(&mut self, artist_ids: Vec<ArtistId<'static>>) {
    match self
      .spotify
      .library_contains(artist_ids.iter().map(|id| LibraryId::Artist(id.as_ref())))
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
      }
      Err(e) => self.handle_error(anyhow!(e)).await,
    }
  }

  async fn set_artists_to_table(&mut self, artists: Vec<FullArtist>) {
    let mut app = self.app.lock().await;
    app.artists = artists;
  }

  async fn get_album_for_track(&mut self, track_id: TrackId<'static>) {
    match self.spotify.track(track_id, None).await {
      Ok(track) => {
        // FullTrack.album is SimplifiedAlbum (not Option) in rspotify 0.14
        let album = track.album;
        self.get_album_tracks(Box::new(album)).await;
      }
      Err(e) => self.handle_error(anyhow!(e)).await,
    }
  }
}
