// Copyright 2015, 2016 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

import React, { Component, PropTypes } from 'react';
import { connect } from 'react-redux';
import { bindActionCreators } from 'redux';
import ContentAdd from 'material-ui/svg-icons/content/add';
import { uniq, isEqual } from 'lodash';

import List from './List';
import { CreateAccount, CreateWallet } from '~/modals';
import { Actionbar, ActionbarExport, ActionbarSearch, ActionbarSort, Button, Page, Tooltip } from '~/ui';
import { setVisibleAccounts } from '~/redux/providers/personalActions';

import styles from './accounts.css';

class Accounts extends Component {
  static contextTypes = {
    api: PropTypes.object
  }

  static propTypes = {
    setVisibleAccounts: PropTypes.func.isRequired,
    accounts: PropTypes.object.isRequired,
    hasAccounts: PropTypes.bool.isRequired,
    wallets: PropTypes.object.isRequired,
    walletsOwners: PropTypes.object.isRequired,
    hasWallets: PropTypes.bool.isRequired,

    balances: PropTypes.object
  }

  state = {
    addressBook: false,
    newDialog: false,
    newWalletDialog: false,
    sortOrder: '',
    searchValues: [],
    searchTokens: [],
    show: false
  }

  componentWillMount () {
    window.setTimeout(() => {
      this.setState({ show: true });
    }, 100);

    this.setVisibleAccounts();
  }

  componentWillReceiveProps (nextProps) {
    const prevAddresses = Object.keys({ ...this.props.accounts, ...this.props.wallets });
    const nextAddresses = Object.keys({ ...nextProps.accounts, ...nextProps.wallets });

    if (prevAddresses.length !== nextAddresses.length || !isEqual(prevAddresses.sort(), nextAddresses.sort())) {
      this.setVisibleAccounts(nextProps);
    }
  }

  componentWillUnmount () {
    this.props.setVisibleAccounts([]);
  }

  setVisibleAccounts (props = this.props) {
    const { accounts, wallets, setVisibleAccounts } = props;
    const addresses = Object.keys({ ...accounts, ...wallets });
    setVisibleAccounts(addresses);
  }

  render () {
    return (
      <div>
        { this.renderNewDialog() }
        { this.renderNewWalletDialog() }
        { this.renderActionbar() }

        <Page>
          <Tooltip
            className={ styles.accountTooltip }
            text='your accounts are visible for easy access, allowing you to edit the meta information, make transfers, view transactions and fund the account'
          />

          { this.renderWallets() }
          { this.renderAccounts() }
        </Page>
      </div>
    );
  }

  renderLoading (object) {
    const loadings = ((object && Object.keys(object)) || []).map((_, idx) => (
      <div key={ idx } className={ styles.loading }>
        <div />
      </div>
    ));

    return (
      <div className={ styles.loadings }>
        { loadings }
      </div>
    );
  }

  renderAccounts () {
    if (!this.state.show) {
      return this.renderLoading(this.props.accounts);
    }

    const { accounts, hasAccounts, balances } = this.props;
    const { searchValues, sortOrder } = this.state;

    return (
      <List
        search={ searchValues }
        accounts={ accounts }
        balances={ balances }
        empty={ !hasAccounts }
        order={ sortOrder }
        handleAddSearchToken={ this.onAddSearchToken } />
    );
  }

  renderWallets () {
    if (!this.state.show) {
      return this.renderLoading(this.props.wallets);
    }

    const { wallets, hasWallets, balances, walletsOwners } = this.props;
    const { searchValues, sortOrder } = this.state;

    if (!wallets || Object.keys(wallets).length === 0) {
      return null;
    }

    return (
      <List
        link='wallet'
        search={ searchValues }
        accounts={ wallets }
        balances={ balances }
        empty={ !hasWallets }
        order={ sortOrder }
        handleAddSearchToken={ this.onAddSearchToken }
        walletsOwners={ walletsOwners }
      />
    );
  }

  renderSearchButton () {
    const onChange = (searchTokens, searchValues) => {
      this.setState({ searchTokens, searchValues });
    };

    return (
      <ActionbarSearch
        key='searchAccount'
        tokens={ this.state.searchTokens }
        onChange={ onChange } />
    );
  }

  renderSortButton () {
    const onChange = (sortOrder) => {
      this.setState({ sortOrder });
    };

    return (
      <ActionbarSort
        key='sortAccounts'
        id='sortAccounts'
        order={ this.state.sortOrder }
        onChange={ onChange } />
    );
  }

  renderActionbar () {
    const { accounts } = this.props;

    const buttons = [
      <Button
        key='newAccount'
        icon={ <ContentAdd /> }
        label='new account'
        onClick={ this.onNewAccountClick } />,

      <Button
        key='newWallet'
        icon={ <ContentAdd /> }
        label='new wallet'
        onClick={ this.onNewWalletClick } />,

      <ActionbarExport
        key='exportAccounts'
        content={ accounts }
        filename='accounts' />,

      this.renderSearchButton(),
      this.renderSortButton()
    ];

    return (
      <Actionbar
        className={ styles.toolbar }
        title='Accounts Overview'
        buttons={ buttons }>
        <Tooltip
          className={ styles.toolbarTooltip }
          right
          text='actions relating to the current view are available on the toolbar for quick access, be it for performing actions or creating a new item' />
      </Actionbar>
    );
  }

  renderNewDialog () {
    const { accounts } = this.props;
    const { newDialog } = this.state;

    if (!newDialog) {
      return null;
    }

    return (
      <CreateAccount
        accounts={ accounts }
        onClose={ this.onNewAccountClose }
        onUpdate={ this.onNewAccountUpdate } />
    );
  }

  renderNewWalletDialog () {
    const { accounts } = this.props;
    const { newWalletDialog } = this.state;

    if (!newWalletDialog) {
      return null;
    }

    return (
      <CreateWallet
        accounts={ accounts }
        onClose={ this.onNewWalletClose }
      />
    );
  }

  onAddSearchToken = (token) => {
    const { searchTokens } = this.state;
    const newSearchTokens = uniq([].concat(searchTokens, token));
    this.setState({ searchTokens: newSearchTokens });
  }

  onNewAccountClick = () => {
    this.setState({
      newDialog: !this.state.newDialog
    });
  }

  onNewWalletClick = () => {
    this.setState({
      newWalletDialog: !this.state.newWalletDialog
    });
  }

  onNewAccountClose = () => {
    this.onNewAccountClick();
  }

  onNewWalletClose = () => {
    this.onNewWalletClick();
  }

  onNewAccountUpdate = () => {
  }
}

function mapStateToProps (state) {
  const { accounts, hasAccounts, wallets, hasWallets, accountsInfo } = state.personal;
  const { balances } = state.balances;
  const walletsInfo = state.wallet.wallets;

  const walletsOwners = Object
    .keys(walletsInfo)
    .map((wallet) => {
      const owners = walletsInfo[wallet].owners || [];

      return {
        owners: owners.map((owner) => ({
          address: owner,
          name: accountsInfo[owner] && accountsInfo[owner].name || owner
        })),
        address: wallet
      };
    })
    .reduce((walletsOwners, wallet) => {
      walletsOwners[wallet.address] = wallet.owners;
      return walletsOwners;
    }, {});

  return {
    accounts,
    hasAccounts,
    wallets,
    walletsOwners,
    hasWallets,
    balances
  };
}

function mapDispatchToProps (dispatch) {
  return bindActionCreators({
    setVisibleAccounts
  }, dispatch);
}

export default connect(
  mapStateToProps,
  mapDispatchToProps
)(Accounts);
