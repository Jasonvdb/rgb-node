// RGB standard library
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use ::core::borrow::Borrow;
use ::core::convert::TryFrom;
use ::std::path::PathBuf;

use lnpbp::lnp::presentation::Encode;
use lnpbp::lnp::zmq::ApiType;
use lnpbp::lnp::{transport, NoEncryption, Session, Unmarshall, Unmarshaller};
use lnpbp::rgb::Genesis;
use lnpbp::TryService;

use super::cache::{Cache, FileCache, FileCacheConfig};
use super::{Asset, IssueStructure};
use super::{Config, Processor};
use crate::api::{
    fungible::{Issue, Request, TransferApi},
    reply, Reply,
};
use crate::error::{
    ApiErrorType, BootstrapError, RuntimeError, ServiceError, ServiceErrorDomain,
    ServiceErrorSource,
};

pub struct Runtime {
    /// Original configuration object
    config: Config,

    /// Request-response API session
    session_rpc: Session<NoEncryption, transport::zmq::Connection>,

    /// Publish-subscribe API session
    session_pub: Session<NoEncryption, transport::zmq::Connection>,

    /// Stash RPC client session
    stash_rpc: Session<NoEncryption, transport::zmq::Connection>,

    /// Publish-subscribe API socket
    stash_sub: Session<NoEncryption, transport::zmq::Connection>,

    /// RGB fungible assets data cache: relational database sharing the client-
    /// friendly asset information with clients
    cacher: FileCache,

    /// Processor instance: handles business logic outside of stash scope
    processor: Processor,

    /// Unmarshaller instance used for parsing RPC request
    unmarshaller: Unmarshaller<Request>,

    /// Unmarshaller instance used for parsing RPC request
    reply_unmarshaller: Unmarshaller<Reply>,
}

impl Runtime {
    /// Internal function for avoiding index-implementation specific function
    /// use and reduce number of errors. Cacher may be switched with compile
    /// configuration options and, thus, we need to make sure that the structure
    /// we use corresponds to certain trait and not specific type.
    fn cache(&self) -> &impl Cache {
        &self.cacher
    }

    pub fn init(config: Config, mut context: &mut zmq::Context) -> Result<Self, BootstrapError> {
        let processor = Processor::new()?;

        let cacher = FileCache::new(FileCacheConfig {
            data_dir: PathBuf::from(&config.cache),
            data_format: config.format,
        })
        .map_err(|err| {
            error!("{}", err);
            err
        })?;

        let session_rpc = Session::new_zmq_unencrypted(
            ApiType::Server,
            &mut context,
            config.rpc_endpoint.clone(),
            None,
        )?;

        let session_pub = Session::new_zmq_unencrypted(
            ApiType::Publish,
            &mut context,
            config.pub_endpoint.clone(),
            None,
        )?;

        let stash_rpc = Session::new_zmq_unencrypted(
            ApiType::Client,
            &mut context,
            config.stash_rpc.clone(),
            None,
        )?;

        let stash_sub = Session::new_zmq_unencrypted(
            ApiType::Subscribe,
            &mut context,
            config.stash_sub.clone(),
            None,
        )?;

        Ok(Self {
            config,
            session_rpc,
            session_pub,
            stash_rpc,
            stash_sub,
            cacher,
            processor,
            unmarshaller: Request::create_unmarshaller(),
            reply_unmarshaller: Reply::create_unmarshaller(),
        })
    }
}

#[async_trait]
impl TryService for Runtime {
    type ErrorType = RuntimeError;

    async fn try_run_loop(mut self) -> Result<!, RuntimeError> {
        loop {
            match self.run().await {
                Ok(_) => debug!("API request processing complete"),
                Err(err) => {
                    error!("Error processing API request: {}", err);
                    Err(err)?;
                }
            }
        }
    }
}

impl Runtime {
    async fn run(&mut self) -> Result<(), RuntimeError> {
        trace!("Awaiting for ZMQ RPC requests...");
        let raw = self.session_rpc.recv_raw_message()?;
        let reply = self.rpc_process(raw).await.unwrap_or_else(|err| err);
        trace!("Preparing ZMQ RPC reply: {:?}", reply);
        let data = reply.encode()?;
        trace!(
            "Sending {} bytes back to the client over ZMQ RPC",
            data.len()
        );
        self.session_rpc.send_raw_message(data)?;
        Ok(())
    }

    async fn rpc_process(&mut self, raw: Vec<u8>) -> Result<Reply, Reply> {
        trace!("Got {} bytes over ZMQ RPC: {:?}", raw.len(), raw);
        let message = &*self
            .unmarshaller
            .unmarshall(&raw)
            .map_err(|err| ServiceError::from_rpc(ServiceErrorSource::Stash, err))?;
        debug!("Received ZMQ RPC request: {:?}", message);
        Ok(match message {
            Request::Issue(issue) => self.rpc_issue(issue).await,
            Request::Transfer(transfer) => self.rpc_transfer(transfer).await,
            Request::ImportAsset(genesis) => self.rpc_import_asset(genesis).await,
            Request::Sync => self.rpc_sync().await,
        }
        .map_err(|err| ServiceError::contract(err, "fungible"))?)
    }

    async fn rpc_issue(&mut self, issue: &Issue) -> Result<Reply, ServiceErrorDomain> {
        debug!("Got ISSUE {}", issue);

        let issue_structure = match issue.inflatable {
            None => IssueStructure::SingleIssue,
            Some(ref seal_spec) => IssueStructure::MultipleIssues {
                max_supply: issue.supply.ok_or(ServiceErrorDomain::Api(
                    ApiErrorType::MissedArgument {
                        request: "Issue".to_string(),
                        argument: "supply".to_string(),
                    },
                ))?,
                reissue_control: seal_spec.clone(),
            },
        };

        let (asset, genesis) = self.processor.issue(
            self.config.network,
            issue.ticker.clone(),
            issue.title.clone(),
            issue.description.clone(),
            issue_structure,
            issue.allocate.clone(),
            issue.precision,
            vec![],
            issue.dust_limit,
        )?;

        self.import_asset(asset, genesis).await?;

        // TODO: Send push request to client informing about cache update

        Ok(Reply::Success)
    }

    async fn rpc_transfer(&mut self, transfer: &TransferApi) -> Result<Reply, ServiceErrorDomain> {
        debug!("Got TRANSFER {}", transfer);

        // TODO: Check inputs that they really exist and have sufficient amount of
        //       asset for the transfer operation

        let mut asset = self.cacher.asset(transfer.contract_id)?.clone();
        let mut psbt = transfer.psbt.clone();
        let _consignment = self.processor.transfer(
            &mut asset,
            &mut psbt,
            transfer.inputs.clone(),
            transfer.ours.clone(),
            transfer.theirs.clone(),
        )?;

        // TODO: Save consignment, send push request etc

        Ok(Reply::Success)
    }

    async fn rpc_sync(&mut self) -> Result<Reply, ServiceErrorDomain> {
        debug!("Got SYNC");
        let data = self.cacher.export()?;
        Ok(Reply::Sync(reply::SyncFormat(self.config.format, data)))
    }

    async fn rpc_import_asset(&mut self, genesis: &Genesis) -> Result<Reply, ServiceErrorDomain> {
        debug!("Got IMPORT_ASSET");
        self.import_asset(Asset::try_from(genesis.clone())?, genesis.clone())
            .await?;
        Ok(Reply::Success)
    }

    async fn import_asset(
        &mut self,
        asset: Asset,
        genesis: Genesis,
    ) -> Result<bool, ServiceErrorDomain> {
        let data = crate::api::stash::Request::AddGenesis(genesis).encode()?;
        self.stash_rpc.send_raw_message(data.borrow())?;
        let raw = self.stash_rpc.recv_raw_message()?;
        if let Reply::Failure(failmsg) = &*self.reply_unmarshaller.unmarshall(&raw)? {
            error!("Failed saving genesis data: {}", failmsg);
            Err(ServiceErrorDomain::Storage)?
        }
        Ok(self.cacher.add_asset(asset)?)
    }
}

pub async fn main_with_config(config: Config) -> Result<(), BootstrapError> {
    let mut context = zmq::Context::new();
    let runtime = Runtime::init(config, &mut context)?;
    runtime.run_or_panic("Fungible contract runtime").await
}
