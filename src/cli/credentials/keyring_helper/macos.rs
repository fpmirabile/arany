use keyring::{Error, Result};
use security_framework::{
    item::{ItemClass, ItemSearchOptions},
    os::macos::keychain::{SecKeychain, SecPreferencesDomain},
};

pub(super) struct Entry {
    keychain: SecKeychain,
    service: String,
    account: String,
    #[cfg(debug_assertions)]
    _interaction: Option<security_framework::os::macos::keychain::KeychainUserInteractionLock>,
}

impl Entry {
    pub(super) fn new(service: &str, account: &str) -> Result<Self> {
        #[cfg(debug_assertions)]
        let fixture = std::env::var_os("ARANY_TEST_KEYCHAIN_ROOT");
        #[cfg(not(debug_assertions))]
        if std::env::var_os("ARANY_TEST_KEYCHAIN_ROOT").is_some() {
            return Err(Error::NoDefaultStore);
        }
        #[cfg(debug_assertions)]
        let (keychain, interaction) = if let Some(root) = fixture {
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
            use cap_std::fs::{Dir, MetadataExt, OpenOptions, OpenOptionsExt};
            let root = arany::StateRoot::open_existing(std::path::Path::new(&root))
                .map_err(|_| Error::NoDefaultStore)?;
            let dir = Dir::open_ambient_dir(root.path(), cap_std::ambient_authority())
                .map_err(|_| Error::NoDefaultStore)?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .follow(FollowSymlinks::No)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
            let file = dir
                .open_with("arany-test.keychain-db", &options)
                .map_err(|_| Error::NoDefaultStore)?;
            let metadata = file.metadata().map_err(|_| Error::NoDefaultStore)?;
            if !metadata.is_file()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
                || metadata.len() > 4 * 1024 * 1024
            {
                return Err(Error::NoDefaultStore);
            }
            let interaction = SecKeychain::disable_user_interaction().map_err(decode_error)?;
            (
                SecKeychain::open(root.path().join("arany-test.keychain-db"))
                    .map_err(decode_error)?,
                Some(interaction),
            )
        } else {
            (
                SecKeychain::default_for_domain(SecPreferencesDomain::User)
                    .map_err(decode_error)?,
                None,
            )
        };
        #[cfg(not(debug_assertions))]
        let keychain =
            SecKeychain::default_for_domain(SecPreferencesDomain::User).map_err(decode_error)?;
        Ok(Self {
            keychain,
            service: service.into(),
            account: account.into(),
            #[cfg(debug_assertions)]
            _interaction: interaction,
        })
    }

    pub(super) fn get_password(&self) -> Result<String> {
        let (bytes, _) = self
            .keychain
            .find_generic_password(&self.service, &self.account)
            .map_err(decode_error)?;
        if bytes.len() > super::slot_record_limit(&self.account).unwrap_or(0) {
            return Err(Error::TooLong(
                "credential".into(),
                super::slot_record_limit(&self.account).unwrap_or(0) as u32,
            ));
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::BadEncoding(Vec::new()))
    }

    pub(super) fn set_password(&self, record: &str) -> Result<()> {
        match self
            .keychain
            .find_generic_password(&self.service, &self.account)
        {
            Ok((_, mut item)) => item.set_password(record.as_bytes()).map_err(decode_error),
            Err(error) if error.code() == -25300 => self
                .keychain
                .add_generic_password(&self.service, &self.account, record.as_bytes())
                .map_err(decode_error),
            Err(error) => Err(decode_error(error)),
        }
    }

    pub(super) fn delete_credential(&self) -> Result<()> {
        ItemSearchOptions::new()
            .keychains(std::slice::from_ref(&self.keychain))
            .class(ItemClass::generic_password())
            .service(&self.service)
            .account(&self.account)
            .delete()
            .map_err(decode_error)
    }
}

fn decode_error(error: security_framework::base::Error) -> Error {
    match error.code() {
        -25300 => Error::NoEntry,
        -61 | -25244 | -25291 | -25292 | -25293 | -25294 | -25295 | -25308 => {
            Error::NoStorageAccess(Box::new(error))
        }
        _ => Error::PlatformFailure(Box::new(error)),
    }
}
