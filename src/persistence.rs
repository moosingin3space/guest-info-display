use std::path::PathBuf;

use rusqlite::{Connection, Result, params};

const APP_DIR: &str = "xyz.mooshq.GuestInfoDisplay";
const DB_FILENAME: &str = "guest-info-display.db";

/// Wi-Fi security protocol. The string representations match the Wi-Fi QR code
/// format (WIFI:T:<security>;...) as specified by ZXing.
#[derive(Debug, Clone, PartialEq)]
pub enum WifiSecurity {
    None,
    Wpa,
}

impl WifiSecurity {
    pub fn qr_type_str(&self) -> &'static str {
        match self {
            WifiSecurity::None => "nopass",
            WifiSecurity::Wpa => "WPA",
        }
    }
}

impl rusqlite::ToSql for WifiSecurity {
    fn to_sql(&self) -> Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.qr_type_str()))
    }
}

impl rusqlite::types::FromSql for WifiSecurity {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_str()? {
            "nopass" => Ok(WifiSecurity::None),
            "WPA" => Ok(WifiSecurity::Wpa),
            other => Err(rusqlite::types::FromSqlError::Other(
                format!("unknown WiFi security type: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WifiCredentials {
    pub ssid: String,
    pub password: String,
    pub security: WifiSecurity,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (or create) the database in `$XDG_DATA_HOME/xyz.mooshq.GuestInfoDisplay/`.
    /// Creates the directory if it does not exist.
    pub fn open() -> Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir).map_err(|_| rusqlite::Error::InvalidPath(dir.clone()))?;

        let conn = Connection::open(dir.join(DB_FILENAME))?;
        let db = Database { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn wifi_credentials(&self) -> Result<Option<WifiCredentials>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ssid, password, security FROM wifi_credentials WHERE id = 1")?;

        match stmt.query_row([], |row| {
            Ok(WifiCredentials {
                ssid: row.get(0)?,
                password: row.get(1)?,
                security: row.get(2)?,
            })
        }) {
            Ok(creds) => Ok(Some(creds)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn set_wifi_credentials(&self, creds: &WifiCredentials) -> Result<()> {
        self.conn.execute(
            "INSERT INTO wifi_credentials (id, ssid, password, security)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 ssid     = excluded.ssid,
                 password = excluded.password,
                 security = excluded.security",
            params![creds.ssid, creds.password, creds.security],
        )?;
        Ok(())
    }

    /// Returns the persisted Spotify Connect device ID, creating one on first launch.
    /// Reusing the same ID keeps a single stable entry in Spotify's device list across restarts.
    pub fn spotify_device_id(&self) -> Result<String> {
        match self.conn.query_row(
            "SELECT device_id FROM spotify_config WHERE id = 1",
            [],
            |row| row.get::<_, String>(0),
        ) {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
                self.conn.execute(
                    "INSERT INTO spotify_config (id, device_id) VALUES (1, ?1)",
                    params![id],
                )?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS wifi_credentials (
                id       INTEGER PRIMARY KEY CHECK (id = 1),
                ssid     TEXT NOT NULL,
                password TEXT NOT NULL,
                security TEXT NOT NULL DEFAULT 'WPA'
            );
            CREATE TABLE IF NOT EXISTS spotify_config (
                id        INTEGER PRIMARY KEY CHECK (id = 1),
                device_id TEXT NOT NULL
            );",
        )
    }
}

/// Returns `$XDG_DATA_HOME/xyz.mooshq.GuestInfoDisplay`, falling back to
/// `~/.local/share/xyz.mooshq.GuestInfoDisplay` when `XDG_DATA_HOME` is unset.
/// In a Flatpak sandbox `$XDG_DATA_HOME` is already scoped to the app, so the
/// subdirectory is redundant but harmless.
fn data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".local/share")
        });
    base.join(APP_DIR)
}
