import { Button, Picker, Text, Textarea, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Kv, Notice, PageHeader, statusTone } from '../../runtime/ui'
import controller from './detail.controller'

const SETTLED = ['completed', 'refunding', 'refunded']

export default function PageView() {
  const { data, event } = useController(controller)
  const { order, loading, error, needsLogin, paying, paymentNotice, curve, curveLoading, curveError, feedbackRatingIndex, feedbackCategoryIndex, feedbackRatings, feedbackCategories, feedbackContent, feedbackBusy, feedbackError, feedbackNotice } = data
  const settled = SETTLED.includes(order?.status)
  return <View className='container'>
    <PageHeader title='订单详情' />
    {needsLogin && <View className='hero'><View className='hero-body'><Text className='hero-title'>登录查看订单</Text><Button type='primary' onClick={event('login', {})}>登录查看订单</Button></View></View>}
    {loading && <Notice>正在加载…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {order && <>
      {paymentNotice && <Notice>{paymentNotice}</Notice>}
      <View className='hero'><View className='hero-body'>
        <Text className='eyebrow'>{order.order_no}</Text>
        <Text className={`tag ${statusTone(order.status)}`}>{order.statusLabel}</Text>
        <View className='hero-figure'>{order.totalText}</View>
        <Text className='hero-desc'>结算费用 · {order.billing_status === 'manual_review' ? '待人工核对' : order.billing_status === 'done' ? '已结算' : '结算状态待确认'}</Text>
      </View></View>
      <View className='card'>
        <Kv label='站点 / 设备'>{order.station_name || order.device_id} · 端口 {order.port_no}</Kv>
        <Kv label='开始'>{order.startedText}</Kv>
        <Kv label='结束'>{order.endedText}</Kv>
        {order.display?.show_energy && <Kv label='电量'>{order.meter_kwh ?? '暂无数据'} {order.display?.hide_unit ? '' : 'kWh'}</Kv>}
      </View>
      <View className='card'>
        {order.display?.show_fee_split && <Kv label='电费'>{order.electricText}</Kv>}
        {order.display?.show_fee_split && <Kv label='服务费'>{order.serviceText}</Kv>}
        <Kv label='预付金额'>{order.paidText}</Kv>
        <Kv label='退款状态'>{order.refundLabel}</Kv>
        <Kv label='已退款金额'>{order.refundedText}</Kv>
        <Kv label='支付单号'>{order.payment_order_no || '—'}</Kv>
      </View>
      {order.billing_status === 'manual_review' && <View className='banner banner-warn'>待核对：缺少可靠计量，管理员核对后结算退款。</View>}
      {order.display?.show_method && order.offer && <View className='card'>{order.offer.mode === 'amount' ? '金额' : order.offer.mode === 'duration' ? '时长' : '电量'} · {order.offer.name}</View>}
      {(order.tariff_lines || []).length > 0 && <View className='card'>{order.tariff_lines.map((item: any, index: number) => <View className='kv' key={String(item ?? index)}><Text>{item}</Text></View>)}</View>}
      {order.rule_description && <View className='card'>{order.rule_description}</View>}
      {(order.card_operations || []).length > 0 && <View className='card'>{order.card_operations.map((item: any, index: number) => <View className='kv' key={String(item?.operation_id ?? index)}>
        <Text className='kv-label'>{item?.kind === 'extend' ? '加时' : '刷卡启动'}</Text>
        <Text>{item?.minutes} 分钟 · {item?.status === 'confirming' ? '确认中' : item?.status === 'confirmed' ? '已确认' : '明确失败，已补偿退款'}</Text>
      </View>)}</View>}
      {order.failure_reason && <View className='banner banner-danger'>异常原因：{order.failure_reason}</View>}
      {settled && <View className='card'>
        <Text className='heading'>历史曲线</Text>
        <Button onClick={event('loadCurve', {})} loading={curveLoading} disabled={curveLoading}>{curve ? '刷新历史曲线' : '查看历史曲线'}</Button>
        {curveError && <View className='error'>{curveError}</View>}
        {curve && <View>
          {!curve.series?.length && <View className='text-secondary'>没有可显示的聚合遥测。</View>}
          {(curve.series || []).map((item: any, index: number) => <View className='curve-row' key={String(item?.bucket_start ?? index)}>
            <View>{item?.bucketText}</View>
            <View>
              {order.display?.show_power && <Text>功率 {item?.power_w_avg ?? '暂无数据'} {order.display?.hide_unit ? '' : 'W'} ·</Text>}
              温度 {item?.temperature_c_avg ?? '暂无数据'} °C
            </View>
            <View>电压 {item?.voltage_v_avg ?? '暂无数据'} V · 电流 {item?.current_a_avg ?? '暂无数据'} A</View>
          </View>)}
          {order.display?.show_energy && curve.summary?.total_kwh != null && <View>总电量：{curve.summary.total_kwh} kWh</View>}
        </View>}
      </View>}
      {settled && !order.feedback_submitted && <View className='card'>
        <Text className='heading'>订单评价与反馈</Text>
        <Picker range={feedbackRatings} value={feedbackRatingIndex} onChange={event('onFeedbackRating', {})}><View className='field'>评分：{feedbackRatings?.[feedbackRatingIndex]} ▾</View></Picker>
        <Picker range={feedbackCategories} value={feedbackCategoryIndex} onChange={event('onFeedbackCategory', {})}><View className='field'>类型：{feedbackCategories?.[feedbackCategoryIndex]} ▾</View></Picker>
        <Text className='field-label'>补充说明</Text>
        <Textarea placeholder='说说充电体验或需要跟进的问题（选填）' maxlength={2000} value={feedbackContent} onInput={event('onFeedbackContent', {})} autoHeight />
        {feedbackError && <View className='error'>{feedbackError}</View>}
        {feedbackNotice && <View className='success'>{feedbackNotice}</View>}
        <Button type='primary' onClick={event('submitFeedback', {})} loading={feedbackBusy} disabled={feedbackBusy}>提交反馈</Button>
      </View>}
      {order.feedback_submitted && <View className='banner banner-ok'>已收到本订单反馈，谢谢。</View>}
      {(order.device_id || settled) && <View className='card'>
        {order.device_id && <Button onClick={event('reportFault', {})}>设备报修</Button>}
        {settled && <Button onClick={event('invoice', {})}>申请发票</Button>}
        {settled && <Button onClick={event('goBill', {})}>查看结算账单</Button>}
      </View>}
      {order.status === 'pending_payment' && <>
        <View className='checkout-spacer' />
        <View className='fixed-checkout'>
          <View><Text className='text-secondary'>待支付</Text><View className='checkout-amount'>{order.totalText}</View></View>
          <Button type='primary' onClick={event('pay', {})} loading={paying} disabled={paying || loading}>继续付款</Button>
        </View>
      </>}
    </>}
  </View>
}
