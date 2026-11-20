import { Button, Input, Picker, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './apply.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { orders, orderLabels, orderIndex, selected, invoiceTypeIndex, invoiceTypes, title, taxNo, email, loading, loadingDetail, submitting, needsLogin, error, notice } = data
  return <View className='container'>
    <PageHeader title='申请发票' subtitle='仅展示最近 100 笔订单中可申请开票的订单' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后申请发票</Text>
      <Button type='primary' onClick={event('login', {})}>微信登录</Button>
    </View></View>}
    {!needsLogin && <View className='card'>
      <Text className='heading'>申请充电订单发票</Text>
      <Text className='field-label'>充电订单</Text>
      <Picker range={orderLabels} value={orderIndex} onChange={event('onOrderSelect', {})}><View className='field'>{orderIndex >= 0 ? orderLabels?.[orderIndex] : '选择已完成的充电订单'}</View></Picker>
      {loadingDetail && <Text className='text-secondary'>正在核对实付金额…</Text>}
      {selected && <View className='entity-amount'>订单实付金额：¥{selected.amountText}</View>}
      <Text className='field-label'>发票类型</Text>
      <Picker range={invoiceTypes} value={invoiceTypeIndex} onChange={event('onType', {})}><View className='field'>发票类型：{invoiceTypes?.[invoiceTypeIndex]} ▾</View></Picker>
      <Text className='field-label'>发票抬头</Text>
      <Input placeholder='发票抬头' maxlength={128} value={title} onInput={event('onTitle', {})} />
      {invoiceTypeIndex === 1 && <>
        <Text className='field-label'>纳税人识别号</Text>
        <Input placeholder='纳税人识别号' maxlength={32} value={taxNo} onInput={event('onTaxNo', {})} />
      </>}
      <Text className='field-label'>接收邮箱（选填）</Text>
      <Input placeholder='接收邮箱（选填）' maxlength={128} value={email} onInput={event('onEmail', {})} />
      <Button type='primary' onClick={event('submit', {})} loading={submitting} disabled={submitting || loadingDetail || !selected}>提交申请</Button>
    </View>}
    {loading && <Notice>正在读取已完成订单…</Notice>}
    {error && <Notice tone='error' retry={event('loadOrders', {})} busy={loading || submitting}>{error}</Notice>}
    {notice && <View className='banner banner-ok'>{notice}</View>}
    {!loading && !error && !needsLogin && !orders?.length && <Empty icon='file-generic'>没有可申请开票的已完成订单</Empty>}
  </View>
}
