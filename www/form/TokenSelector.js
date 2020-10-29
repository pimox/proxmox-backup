Ext.define('PBS.form.TokenSelector', {
    extend: 'Proxmox.form.ComboGrid',
    alias: 'widget.pbsTokenSelector',

    allowBlank: false,
    autoSelect: false,
    valueField: 'tokenid',
    displayField: 'tokenid',

    editable: true,
    anyMatch: true,
    forceSelection: true,

    store: {
	model: 'pbs-tokens',
	params: {
	    enabled: 1,
	},
	sorters: 'tokenid',
    },

    initComponent: function() {
	let me = this;
	me.userStore = Ext.create('Ext.data.Store', {
	    model: 'pbs-users-with-tokens',
	});
	me.userStore.on('load', this.onLoad, this);
	me.userStore.load();

	me.callParent();
    },

    onLoad: function(store, data, success) {
	if (!success) return;

	let tokenStore = this.store;

	let records = [];
	Ext.Array.each(data, function(user) {
	let tokens = user.data.tokens || [];
	Ext.Array.each(tokens, function(token) {
	    let r = {};
	    r.tokenid = token.tokenid;
	    r.comment = token.comment;
	    r.expire = token.expire;
	    r.enable = token.enable;
	    records.push(r);
	});
	});

	tokenStore.loadData(records);
    },

    listConfig: {
	columns: [
	    {
		header: gettext('API Token'),
		sortable: true,
		dataIndex: 'tokenid',
		renderer: Ext.String.htmlEncode,
		flex: 1,
	    },
	    {
		header: gettext('Comment'),
		sortable: false,
		dataIndex: 'comment',
		renderer: Ext.String.htmlEncode,
		flex: 1,
	    },
	],
    },
});